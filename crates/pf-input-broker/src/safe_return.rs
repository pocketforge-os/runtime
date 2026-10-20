//! The broker as the protected System Menu router and SafeReturn fallback intake.
//!
//! While an app is foreground the broker holds the pad's `EVIOCGRAB`, so it is the one process
//! that can see guide/Menu (`BTN_MODE`, `0x13c`) without trusting the app. With
//! `--safe-return-sock PATH` it drops every guide transition from the re-emit stream, so the app
//! never observes it. A press is offered to the registered System Menu provider. Missing,
//! disconnected, invalid, or unresponsive providers fall back to the existing SafeReturn intake
//! after 250 ms. After acknowledging, a provider may report `shown` when its frame is committed;
//! v1 accepts and logs that message without enforcing a shown deadline or health policy. The
//! menu's Return request enters the same SafeReturn intake.
//!
//! The authority is single-threaded, and while it runs `systemctl stop` it may block for 2 s or
//! more. The input pump therefore never touches the socket:
//! * a guide press only flips an atomic and hands a token to one persistent worker thread;
//! * the worker connects with a 250 ms deadline and waits at most 5 s for the reply;
//! * presses that arrive while a request is in flight are coalesced, not queued.
//!
//! `pf-session-client`'s `SocketTransport` is deliberately NOT used. It has no connect or read
//! timeouts, and the launcher vendors that crate, so adding timeouts there would change a
//! launcher contract crate.

use std::io;
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::remap::{Posture, Rotation};

/// The canonical guide/Menu key (`BTN_MODE`) the broker reserves for System Menu routing.
pub const GUIDE_CODE: u16 = 0x13c;

/// Connect deadline for the authority socket.
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);

/// Read (and write) deadline for one SafeReturn round trip. It covers an authority that is busy
/// running `systemctl stop` for the app's `TimeoutStopSec`.
pub const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// The pf-wire frame body: exactly `serde_json::to_vec(&RpcRequest::SafeReturn)`.
pub const SAFE_RETURN_BODY: &[u8] = br#"{"method":"safe_return"}"#;

const ORIENTATION_RPC_VERSION: u32 = 1;
const ORIENTATION_WAIT: Duration = Duration::from_secs(1);

/// The broker's nonblocking view of the session authority's resolved body posture.
///
/// The worker owns all socket I/O. The report pump only performs one atomic load before a
/// report, so an unavailable or slow authority cannot stall input delivery. The value starts at
/// Normal and is replaced only by a complete, version-checked orientation response.
pub struct PostureFeed {
    posture: Arc<std::sync::atomic::AtomicU8>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

/// Test-only publisher for a synthetic authority snapshot.
#[cfg(test)]
pub struct PosturePublisher {
    posture: Arc<std::sync::atomic::AtomicU8>,
}

impl PostureFeed {
    /// Start consuming orientation v1 snapshots from the authority socket.
    pub fn spawn(sock: impl Into<PathBuf>) -> io::Result<Self> {
        Self::spawn_with(sock, Box::new(|line| eprintln!("{line}")))
    }

    /// Start the bounded orientation worker with an explicit log sink.
    pub fn spawn_with(sock: impl Into<PathBuf>, log: LogSink) -> io::Result<Self> {
        let posture = Arc::new(std::sync::atomic::AtomicU8::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_posture = posture.clone();
        let worker_stop = stop.clone();
        let sock = sock.into();
        let worker = std::thread::Builder::new()
            .name("pf-orientation".into())
            .spawn(move || orientation_worker(&sock, worker_posture, worker_stop, log))?;
        Ok(Self {
            posture,
            stop,
            worker: Some(worker),
        })
    }

    /// Read the latest authority posture without performing I/O.
    pub(crate) fn current(&self) -> Posture {
        posture_from_code(self.posture.load(Ordering::Acquire))
    }

    #[cfg(test)]
    pub(crate) fn synthetic() -> (Self, PosturePublisher) {
        let posture = Arc::new(std::sync::atomic::AtomicU8::new(0));
        (
            Self {
                posture: posture.clone(),
                stop: Arc::new(AtomicBool::new(false)),
                worker: None,
            },
            PosturePublisher { posture },
        )
    }
}

impl Drop for PostureFeed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
impl PosturePublisher {
    pub(crate) fn publish(&self, posture: Posture) {
        self.posture
            .store(posture_to_code(posture), Ordering::Release);
    }
}

fn orientation_worker(
    sock: &Path,
    posture: Arc<std::sync::atomic::AtomicU8>,
    stop: Arc<AtomicBool>,
    log: LogSink,
) {
    let mut serial = None;
    let mut unavailable = false;
    while !stop.load(Ordering::Acquire) {
        match read_orientation(sock, serial) {
            Ok((next_serial, next_posture)) => {
                serial = Some(next_serial);
                posture.store(posture_to_code(next_posture), Ordering::Release);
                if unavailable {
                    log(&format!(
                        "pf-input-broker: orientation_source_recovered source={} posture={next_posture:?}",
                        sock.display()
                    ));
                    unavailable = false;
                }
            }
            Err(error) => {
                if !unavailable {
                    log(&format!(
                        "pf-input-broker: orientation_source_unavailable source={} reason={:?} fallback=last_valid_or_normal",
                        sock.display(),
                        error.kind()
                    ));
                    unavailable = true;
                }
                for _ in 0..25 {
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
}

fn read_orientation(sock: &Path, after_serial: Option<u64>) -> io::Result<(u64, Posture)> {
    let mut stream = connect_with_timeout(sock, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut request = serde_json::json!({
        "method": "orientation",
        "version": ORIENTATION_RPC_VERSION,
        "wait_ms": ORIENTATION_WAIT.as_millis() as u64,
    });
    if let Some(serial) = after_serial {
        request["after_serial"] = serde_json::Value::from(serial);
    }
    let request = serde_json::to_vec(&request)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    pf_wire::write_frame(&mut stream, &request).map_err(wire_io)?;
    let reply = pf_wire::read_frame(&mut stream).map_err(wire_io)?;
    parse_orientation(&reply)
}

fn parse_orientation(reply: &[u8]) -> io::Result<(u64, Posture)> {
    let value: serde_json::Value = serde_json::from_slice(reply)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if value.get("result").and_then(serde_json::Value::as_str) != Some("orientation") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "authority did not return an orientation snapshot",
        ));
    }
    if value.get("version").and_then(serde_json::Value::as_u64)
        != Some(ORIENTATION_RPC_VERSION as u64)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported orientation response version",
        ));
    }
    let state = value
        .get("state")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "orientation state missing"))?;
    let serial = state
        .get("serial")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "orientation serial missing"))?;
    let whole_body = state
        .get("whole_body")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "whole_body posture missing"))?;
    let posture = match whole_body {
        "normal" => Posture::Normal,
        // Authority's left/right names describe the user-facing turn. Remap's Rotation names
        // describe the physical body clockwise, hence the intentional inverse here.
        "left" => Posture::WholeBodyRotated(Rotation::Cw270),
        "upsidedown" => Posture::WholeBodyRotated(Rotation::Cw180),
        "right" => Posture::WholeBodyRotated(Rotation::Cw90),
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown whole_body posture {other:?}"),
            ))
        }
    };
    Ok((serial, posture))
}

fn posture_to_code(posture: Posture) -> u8 {
    match posture {
        Posture::Normal | Posture::ScreenOnlyRotated(_) => 0,
        Posture::WholeBodyRotated(Rotation::Cw90) => 1,
        Posture::WholeBodyRotated(Rotation::Cw180) => 2,
        Posture::WholeBodyRotated(Rotation::Cw270) => 3,
        Posture::WholeBodyRotated(Rotation::Cw0) => 0,
    }
}

fn posture_from_code(code: u8) -> Posture {
    match code {
        1 => Posture::WholeBodyRotated(Rotation::Cw90),
        2 => Posture::WholeBodyRotated(Rotation::Cw180),
        3 => Posture::WholeBodyRotated(Rotation::Cw270),
        _ => Posture::Normal,
    }
}

/// Where the worker reports each round trip's outcome, one line per request.
pub type LogSink = Box<dyn Fn(&str) + Send + 'static>;

/// Handle to the SafeReturn worker. Cheap to call from the input pump: [`request`](Self::request)
/// never blocks and never performs I/O.
#[derive(Clone)]
pub struct SafeReturnIntake {
    tx: SyncSender<()>,
    busy: Arc<AtomicBool>,
}

/// Maximum time a registered provider may take to acknowledge `system_menu` ownership.
pub const SYSTEM_MENU_ACK_TIMEOUT: Duration = Duration::from_millis(250);

/// Versioned v1 provider protocol frames. Unknown optional JSON fields are ignored; changing the
/// meaning of these fields or making a new field mandatory requires a new major version.
pub const SYSTEM_MENU_REGISTER_BODY: &[u8] = br#"{"version":1,"type":"register"}"#;
pub const SYSTEM_MENU_REGISTERED_BODY: &[u8] = br#"{"version":1,"type":"registered"}"#;
pub const SYSTEM_MENU_ACTION_BODY: &[u8] = br#"{"version":1,"type":"system_menu"}"#;
pub const SYSTEM_MENU_ACK_BODY: &[u8] = br#"{"version":1,"type":"ack","action":"system_menu"}"#;
pub const SYSTEM_MENU_SHOWN_BODY: &[u8] = br#"{"version":1,"type":"shown"}"#;
pub const SYSTEM_MENU_RETURN_BODY: &[u8] = br#"{"version":1,"type":"return"}"#;
pub const SYSTEM_MENU_RETURNED_BODY: &[u8] = br#"{"version":1,"type":"ack","action":"return"}"#;

struct RegisteredProvider {
    writer: UnixStream,
    replies: Receiver<io::Result<Vec<u8>>>,
}

impl RegisteredProvider {
    fn shutdown(&self) {
        let _ = self.writer.shutdown(Shutdown::Both);
    }
}

enum SystemMenuCommand {
    Register {
        provider: UnixStream,
        ready: SyncSender<io::Result<()>>,
    },
    Press,
    /// FIFO barrier: answered only after every command enqueued before it was fully processed.
    Sync(SyncSender<()>),
}

/// Routes protected Menu presses to one registered provider, with fail-safe return fallback.
#[derive(Clone)]
pub struct SystemMenuRouter {
    safe_return: SafeReturnIntake,
    tx: SyncSender<SystemMenuCommand>,
    busy: Arc<AtomicBool>,
}

impl SystemMenuRouter {
    pub fn spawn(safe_return: SafeReturnIntake) -> io::Result<Self> {
        Self::spawn_with(
            safe_return,
            SYSTEM_MENU_ACK_TIMEOUT,
            Box::new(|line| eprintln!("{line}")),
        )
    }

    pub fn spawn_with(
        safe_return: SafeReturnIntake,
        ack_timeout: Duration,
        log: Box<dyn Fn(&str) + Send + Sync + 'static>,
    ) -> io::Result<Self> {
        let (tx, rx) = sync_channel(2);
        let busy = Arc::new(AtomicBool::new(false));
        let worker_busy = busy.clone();
        let fallback = safe_return.clone();
        let log: Arc<dyn Fn(&str) + Send + Sync> = Arc::from(log);
        std::thread::Builder::new()
            .name("pf-system-menu".into())
            .spawn(move || {
                let mut provider: Option<RegisteredProvider> = None;
                while let Ok(command) = rx.recv() {
                    match command {
                        SystemMenuCommand::Register {
                            provider: candidate,
                            ready,
                        } => {
                            let result = prepare_provider(candidate, log.clone());
                            match result {
                                Ok(candidate) => {
                                    if let Some(previous) = provider.replace(candidate) {
                                        previous.shutdown();
                                    }
                                    let _ = ready.send(Ok(()));
                                }
                                Err(error) => {
                                    let _ = ready.send(Err(error));
                                }
                            }
                        }
                        SystemMenuCommand::Press => {
                            let outcome = provider
                                .as_mut()
                                .ok_or_else(|| {
                                    io::Error::new(
                                        io::ErrorKind::NotConnected,
                                        "no system-menu provider registered",
                                    )
                                })
                                .and_then(|provider| send_system_menu(provider, ack_timeout));
                            // Re-arm the busy gate after the fallback dispatch but before the
                            // log line is emitted, so a waiter that observed the log line also
                            // observes `in_flight() == false` (channel happens-before). Hermetic
                            // tests synchronize on the log line instead of polling the flag.
                            match outcome {
                                Ok(()) => {
                                    worker_busy.store(false, Ordering::Release);
                                    log("pf-input-broker: system_menu acknowledged");
                                }
                                Err(error) => {
                                    if let Some(failed) = provider.take() {
                                        failed.shutdown();
                                    }
                                    let dispatched = fallback.request();
                                    worker_busy.store(false, Ordering::Release);
                                    log(&format!(
                                        "pf-input-broker: system_menu fallback to safe_return ({:?}): {error}; dispatched={dispatched}",
                                        error.kind()
                                    ));
                                }
                            }
                        }
                        SystemMenuCommand::Sync(ready) => {
                            let _ = ready.send(());
                        }
                    }
                }
            })?;
        Ok(Self {
            safe_return,
            tx,
            busy,
        })
    }

    /// Install one already-authenticated provider connection. A new provider replaces the old
    /// one. The registration acknowledgement is written before this method returns.
    pub fn register_provider(&self, provider: UnixStream) -> io::Result<()> {
        let (ready_tx, ready_rx) = sync_channel(0);
        self.tx
            .send(SystemMenuCommand::Register {
                provider,
                ready: ready_tx,
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "system-menu worker stopped"))?;
        ready_rx
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "system-menu worker stopped"))?
    }

    /// Queue one protected Menu press without blocking the input pump. Concurrent presses are
    /// coalesced. If the worker cannot accept the press, invoke the fail-safe path directly.
    pub fn press(&self) -> bool {
        if self.busy.swap(true, Ordering::AcqRel) {
            return false;
        }
        match self.tx.try_send(SystemMenuCommand::Press) {
            Ok(()) => true,
            Err(TrySendError::Full(SystemMenuCommand::Press))
            | Err(TrySendError::Disconnected(SystemMenuCommand::Press)) => {
                self.busy.store(false, Ordering::Release);
                self.safe_return.request()
            }
            Err(TrySendError::Full(SystemMenuCommand::Register { .. }))
            | Err(TrySendError::Disconnected(SystemMenuCommand::Register { .. }))
            | Err(TrySendError::Full(SystemMenuCommand::Sync(_)))
            | Err(TrySendError::Disconnected(SystemMenuCommand::Sync(_))) => {
                unreachable!("press sends only Press")
            }
        }
    }

    /// The menu's Return item uses the exact same authority intake as fail-safe fallback.
    pub fn return_item(&self) -> bool {
        self.safe_return.request()
    }

    /// FIFO barrier over the worker's command queue: once this returns, every press enqueued
    /// before the call has been fully processed (its provider outcome observed, its fallback
    /// dispatch decided, and the busy gate re-armed). Hermetic tests use it to replace
    /// wall-clock polling windows with explicit synchronisation; the input pump never calls it.
    pub fn sync(&self) -> io::Result<()> {
        let (ready_tx, ready_rx) = sync_channel(0);
        self.tx
            .send(SystemMenuCommand::Sync(ready_tx))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "system-menu worker stopped"))?;
        ready_rx
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "system-menu worker stopped"))
    }

    pub fn in_flight(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    /// `true` while a fail-safe or menu-item return is queued at the authority intake.
    pub fn fallback_in_flight(&self) -> bool {
        self.safe_return.in_flight()
    }
}

fn prepare_provider(
    mut candidate: UnixStream,
    log: Arc<dyn Fn(&str) + Send + Sync>,
) -> io::Result<RegisteredProvider> {
    candidate.set_write_timeout(Some(SYSTEM_MENU_ACK_TIMEOUT))?;
    pf_wire::write_frame(&mut candidate, SYSTEM_MENU_REGISTERED_BODY).map_err(wire_io)?;
    let mut reader = candidate.try_clone()?;
    reader.set_read_timeout(None)?;
    let (reply_tx, replies) = channel();
    std::thread::Builder::new()
        .name("pf-system-menu-provider".into())
        .spawn(move || loop {
            match pf_wire::read_frame(&mut reader).map_err(wire_io) {
                Ok(body) => match is_shown(&body) {
                    Ok(true) => log("pf-input-broker: system_menu shown"),
                    Ok(false) => {
                        if reply_tx.send(Ok(body)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = reply_tx.send(Err(error));
                        break;
                    }
                },
                Err(error) => {
                    let _ = reply_tx.send(Err(error));
                    break;
                }
            }
        })?;
    Ok(RegisteredProvider {
        writer: candidate,
        replies,
    })
}

fn is_shown(body: &[u8]) -> io::Result<bool> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(
        value.get("version").and_then(serde_json::Value::as_u64) == Some(1)
            && value.get("type").and_then(serde_json::Value::as_str) == Some("shown"),
    )
}

fn send_system_menu(provider: &mut RegisteredProvider, ack_timeout: Duration) -> io::Result<()> {
    pf_wire::write_frame(&mut provider.writer, SYSTEM_MENU_ACTION_BODY).map_err(wire_io)?;
    let reply = provider
        .replies
        .recv_timeout(ack_timeout)
        .map_err(|error| match error {
            std::sync::mpsc::RecvTimeoutError::Timeout => io::Error::new(
                io::ErrorKind::TimedOut,
                "system_menu acknowledgement timed out",
            ),
            std::sync::mpsc::RecvTimeoutError::Disconnected => io::Error::new(
                io::ErrorKind::BrokenPipe,
                "system-menu provider disconnected",
            ),
        })??;
    let value: serde_json::Value = serde_json::from_slice(&reply)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let valid = value.get("version").and_then(serde_json::Value::as_u64) == Some(1)
        && value.get("type").and_then(serde_json::Value::as_str) == Some("ack")
        && value.get("action").and_then(serde_json::Value::as_str) == Some("system_menu");
    if valid {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "invalid system_menu acknowledgement: {}",
                String::from_utf8_lossy(&reply)
            ),
        ))
    }
}

/// Handle one authenticated provider endpoint connection. `register` transfers the persistent
/// connection to the router; `return` is a one-shot invocation of the shared return path.
pub fn serve_system_menu_provider(
    mut stream: UnixStream,
    router: &SystemMenuRouter,
    trusted_uid: u32,
) -> io::Result<()> {
    if peer_uid(&stream)? != trusted_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "system-menu provider uid is not trusted",
        ));
    }
    stream.set_read_timeout(Some(SYSTEM_MENU_ACK_TIMEOUT))?;
    stream.set_write_timeout(Some(SYSTEM_MENU_ACK_TIMEOUT))?;
    let request = pf_wire::read_frame(&mut stream).map_err(wire_io)?;
    let value: serde_json::Value = serde_json::from_slice(&request)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if value.get("version").and_then(serde_json::Value::as_u64) != Some(1) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported system-menu provider protocol version",
        ));
    }
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("register") => router.register_provider(stream),
        Some("return") => {
            let _ = router.return_item();
            pf_wire::write_frame(&mut stream, SYSTEM_MENU_RETURNED_BODY).map_err(wire_io)
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown system-menu provider request",
        )),
    }
}

pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(credentials.uid)
    }
}

impl SafeReturnIntake {
    /// Start the worker for `sock` with the production timeouts, logging to stderr.
    pub fn spawn(sock: impl Into<PathBuf>) -> io::Result<SafeReturnIntake> {
        SafeReturnIntake::spawn_with(
            sock,
            CONNECT_TIMEOUT,
            IO_TIMEOUT,
            Box::new(|line| eprintln!("{line}")),
        )
    }

    /// Start the worker with explicit timeouts and log sink (the seam hermetic tests use).
    pub fn spawn_with(
        sock: impl Into<PathBuf>,
        connect_timeout: Duration,
        io_timeout: Duration,
        log: LogSink,
    ) -> io::Result<SafeReturnIntake> {
        let sock = sock.into();
        let busy = Arc::new(AtomicBool::new(false));
        // Capacity 1 is enough: `busy` admits at most one token until the worker finishes it.
        let (tx, rx) = sync_channel::<()>(1);
        let worker_busy = busy.clone();
        std::thread::Builder::new()
            .name("pf-safe-return".into())
            .spawn(move || {
                // Ends when the intake (the only sender) is dropped.
                while rx.recv().is_ok() {
                    let outcome = send_safe_return(&sock, connect_timeout, io_timeout);
                    // Re-arm the gate before emitting the outcome line, so a waiter that
                    // observed the line also observes `in_flight() == false` (channel
                    // happens-before). Hermetic tests synchronize on the log line instead of
                    // polling the flag.
                    worker_busy.store(false, Ordering::Release);
                    log(&outcome_line(&sock, &outcome));
                }
            })?;
        Ok(SafeReturnIntake { tx, busy })
    }

    /// Ask the worker to send one SafeReturn. Returns `true` when a request was dispatched and
    /// `false` when it was coalesced into the request already in flight.
    pub fn request(&self) -> bool {
        if self.busy.swap(true, Ordering::AcqRel) {
            return false;
        }
        match self.tx.try_send(()) {
            Ok(()) => true,
            Err(TrySendError::Full(())) | Err(TrySendError::Disconnected(())) => {
                // Unreachable while the worker lives (busy admits one token at a time). Never
                // leave the gate latched, or every later press would be silently coalesced.
                self.busy.store(false, Ordering::Release);
                eprintln!("pf-input-broker: safe_return worker unavailable; press dropped");
                false
            }
        }
    }

    /// `true` while a request is queued or in flight.
    pub fn in_flight(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }
}

/// Tracks guide-key state and turns press edges into protected System Menu routing. Every guide
/// transition is consumed, so the foreground app never observes it.
pub struct SystemMenuGate {
    router: SystemMenuRouter,
    down: bool,
}

impl SystemMenuGate {
    pub fn new(router: SystemMenuRouter) -> SystemMenuGate {
        SystemMenuGate {
            router,
            down: false,
        }
    }

    /// Offer one canonical `EV_KEY` transition. Returns `true` when it is the guide key, which
    /// the caller must then drop from the re-emit stream. Autorepeat (`value == 2`) keeps the key
    /// down and is not a new press.
    pub fn consume_key(&mut self, code: u16, value: i32) -> bool {
        if code != GUIDE_CODE {
            return false;
        }
        self.set_down(value != 0);
        true
    }

    /// Apply the guide key's authoritative state, for example after a `SYN_DROPPED` resync. An
    /// up-to-down change is a press edge.
    pub fn set_down(&mut self, down: bool) {
        if down && !self.down {
            self.router.press();
        }
        self.down = down;
    }

    pub fn router(&self) -> &SystemMenuRouter {
        &self.router
    }
}

/// One SafeReturn round trip: connect (bounded), write the frame, read the authority's reply
/// frame (bounded), and require `{"result":"ok"}`.
pub fn send_safe_return(
    sock: &Path,
    connect_timeout: Duration,
    io_timeout: Duration,
) -> io::Result<()> {
    let mut stream = connect_with_timeout(sock, connect_timeout)?;
    stream.set_read_timeout(Some(io_timeout))?;
    stream.set_write_timeout(Some(io_timeout))?;
    pf_wire::write_frame(&mut stream, SAFE_RETURN_BODY).map_err(wire_io)?;
    let reply = pf_wire::read_frame(&mut stream).map_err(wire_io)?;
    let value: serde_json::Value = serde_json::from_slice(&reply)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if value.get("result").and_then(serde_json::Value::as_str) == Some("ok") {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "authority answered {}",
            String::from_utf8_lossy(&reply)
        )))
    }
}

fn outcome_line(sock: &Path, outcome: &io::Result<()>) -> String {
    match outcome {
        Ok(()) => format!(
            "pf-input-broker: safe_return sent to {}: ok",
            sock.display()
        ),
        Err(e) => format!(
            "pf-input-broker: safe_return to {} failed ({:?}): {e}",
            sock.display(),
            e.kind()
        ),
    }
}

fn wire_io(e: pf_wire::WireError) -> io::Error {
    match e {
        pf_wire::WireError::Io(e) => e,
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}

/// `connect(2)` to a Unix stream socket with a deadline. std's `UnixStream::connect` has none: it
/// can block when the listener's backlog is full (a wedged, single-threaded authority). A
/// non-blocking AF_UNIX connect reports a full backlog as `EAGAIN`, so it is retried on a fresh
/// socket until the deadline.
pub fn connect_with_timeout(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    let deadline = Instant::now() + timeout;
    let (addr, len) = sockaddr_un(path)?;
    loop {
        // SAFETY: plain socket(2); the result is checked before it is owned.
        let raw = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: raw is a fresh, owned fd.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: addr/len describe a valid, initialized sockaddr_un.
        let rc = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&addr as *const libc::sockaddr_un).cast(),
                len,
            )
        };
        if rc == 0 {
            return into_blocking_stream(fd);
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINPROGRESS) => {
                wait_connected(&fd, deadline)?;
                return into_blocking_stream(fd);
            }
            Some(libc::EAGAIN) | Some(libc::EINTR) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(timed_out());
                }
                std::thread::sleep((deadline - now).min(Duration::from_millis(10)));
            }
            _ => return Err(err),
        }
    }
}

fn wait_connected(fd: &OwnedFd, deadline: Instant) -> io::Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timed_out());
        }
        let mut pfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        let ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        // SAFETY: one valid pollfd.
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(e);
        }
        if rc == 0 {
            return Err(timed_out());
        }
        let mut so_error: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: getsockopt writes one c_int into so_error.
        let rc = unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&mut so_error as *mut libc::c_int).cast(),
                &mut len,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        return if so_error == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(so_error))
        };
    }
}

fn into_blocking_stream(fd: OwnedFd) -> io::Result<UnixStream> {
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(false)?;
    Ok(stream)
}

fn timed_out() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "connect to session authority timed out",
    )
}

fn sockaddr_un(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let bytes = path.as_os_str().as_encoded_bytes();
    // SAFETY: sockaddr_un is plain old data; all-zero is a valid initial value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.is_empty() || bytes.contains(&0) || bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Unix socket path",
        ));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let len = std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1;
    Ok((addr, len as libc::socklen_t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc::{channel, Receiver};

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pf-safe-return-{tag}-{}.sock", std::process::id()))
    }

    fn logging_intake(sock: &Path, io_timeout: Duration) -> (SafeReturnIntake, Receiver<String>) {
        let (tx, rx) = channel();
        let intake = SafeReturnIntake::spawn_with(
            sock,
            CONNECT_TIMEOUT,
            io_timeout,
            Box::new(move |line| {
                let _ = tx.send(line.to_owned());
            }),
        )
        .unwrap();
        (intake, rx)
    }

    /// Block until the next log line arrives. The bound is generous (10 s) and is only ever
    /// reached when the behaviour under test regressed; healthy runs resolve in milliseconds,
    /// so this is an event-driven wait, not a timing window.
    fn wait_for_any_line(rx: &Receiver<String>) -> String {
        rx.recv_timeout(Duration::from_secs(10))
            .expect("log line did not arrive within 10 s")
    }

    /// Block until a log line containing `needle` arrives; other lines are discarded. Both
    /// workers re-arm their busy gate BEFORE emitting their outcome line, so receiving a line
    /// proves the corresponding `in_flight()` flag is already false (channel happens-before) —
    /// tests never poll the flag on a wall-clock window.
    fn wait_for_line(rx: &Receiver<String>, needle: &str) -> String {
        loop {
            let line = wait_for_any_line(rx);
            if line.contains(needle) {
                return line;
            }
        }
    }

    /// A router whose log lines go to a channel so tests can synchronize on worker completion.
    fn logging_router(
        intake: SafeReturnIntake,
        ack_timeout: Duration,
    ) -> (SystemMenuRouter, Receiver<String>) {
        let (tx, rx) = channel();
        let router = SystemMenuRouter::spawn_with(
            intake,
            ack_timeout,
            Box::new(move |line| {
                let _ = tx.send(line.to_owned());
            }),
        )
        .unwrap();
        (router, rx)
    }

    /// Wait for one SafeReturn connection, answer `{"result":"ok"}`, return the request body.
    /// Event-driven bounded wait: returns as soon as the connection arrives (the 2 ms pause
    /// only yields the CPU while polling); the generous bound is reached solely when the
    /// dispatch under test regressed.
    fn accept_safe_return(listener: &UnixListener, timeout: Duration) -> Vec<u8> {
        listener.set_nonblocking(true).unwrap();
        let started = Instant::now();
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    let body = pf_wire::read_frame(&mut stream).unwrap();
                    pf_wire::write_frame(&mut stream, br#"{"result":"ok"}"#).unwrap();
                    return body;
                }
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock && started.elapsed() < timeout =>
                {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("safe_return was not received: {error}"),
            }
        }
    }

    fn register_test_provider(router: &SystemMenuRouter) -> UnixStream {
        let (mut provider, broker) = UnixStream::pair().unwrap();
        let router = router.clone();
        let trusted_uid = unsafe { libc::geteuid() };
        let server =
            std::thread::spawn(move || serve_system_menu_provider(broker, &router, trusted_uid));
        pf_wire::write_frame(
            &mut provider,
            br#"{"version":1,"type":"register","future_optional":true}"#,
        )
        .unwrap();
        server.join().unwrap().unwrap();
        assert_eq!(
            pf_wire::read_frame(&mut provider).unwrap(),
            SYSTEM_MENU_REGISTERED_BODY
        );
        provider
    }

    #[test]
    fn menu_without_provider_uses_safe_return() {
        let sock = scratch("menu-no-provider");
        let _ = std::fs::remove_file(&sock);
        let authority = UnixListener::bind(&sock).unwrap();
        let (intake, _log) = logging_intake(&sock, IO_TIMEOUT);
        let router =
            SystemMenuRouter::spawn_with(intake, Duration::from_millis(50), Box::new(|_| {}))
                .unwrap();

        assert!(router.press());

        let body = accept_safe_return(&authority, Duration::from_secs(10));
        assert_eq!(body, SAFE_RETURN_BODY);
        let _ = std::fs::remove_file(sock);
    }

    #[test]
    fn acknowledging_provider_gets_system_menu_without_return() {
        let sock = scratch("menu-acked-provider");
        let _ = std::fs::remove_file(&sock);
        let authority = UnixListener::bind(&sock).unwrap();
        authority.set_nonblocking(true).unwrap();
        let (intake, _log) = logging_intake(&sock, IO_TIMEOUT);
        let (router, router_log) = logging_router(intake, Duration::from_millis(50));
        let mut provider = register_test_provider(&router);
        provider
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();

        assert!(router.press());
        let action = pf_wire::read_frame(&mut provider).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&action).unwrap(),
            serde_json::json!({"version": 1, "type": "system_menu"})
        );
        pf_wire::write_frame(
            &mut provider,
            br#"{"version":1,"type":"ack","action":"system_menu"}"#,
        )
        .unwrap();
        // The "acknowledged" line is emitted after the worker re-armed the busy gate, so
        // receiving it proves the press completed — no polling window. A regressed dispatch
        // of the fallback would have set the intake gate synchronously BEFORE the line was
        // logged, so the flag assert is the deterministic negative control; the backlog
        // check below is only a backstop (the dispatched connect lands asynchronously).
        wait_for_line(&router_log, "acknowledged");
        assert!(
            !router.in_flight(),
            "provider acknowledgement was not observed"
        );
        assert!(
            !router.fallback_in_flight(),
            "an acknowledged provider must suppress fallback return"
        );
        // Zero-window backstop: the router worker (the only would-be client) is provably
        // idle, so a connection in the backlog could only come from a regressed dispatch.
        assert_eq!(
            authority.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock,
            "an acknowledged provider must suppress fallback return"
        );
        let _ = std::fs::remove_file(sock);
    }

    #[test]
    fn shown_after_ack_is_accepted_and_logged_without_poisoning_the_provider() {
        let sock = scratch("menu-shown-provider");
        let _ = std::fs::remove_file(&sock);
        let authority = UnixListener::bind(&sock).unwrap();
        authority.set_nonblocking(true).unwrap();
        let (intake, _return_log) = logging_intake(&sock, IO_TIMEOUT);
        let (router, menu_log_rx) = logging_router(intake, Duration::from_millis(50));
        let mut provider = register_test_provider(&router);
        provider
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();

        assert!(router.press());
        assert_eq!(
            pf_wire::read_frame(&mut provider).unwrap(),
            SYSTEM_MENU_ACTION_BODY
        );
        pf_wire::write_frame(&mut provider, SYSTEM_MENU_ACK_BODY).unwrap();
        pf_wire::write_frame(&mut provider, SYSTEM_MENU_SHOWN_BODY).unwrap();
        // "shown" is logged by the provider reader thread and "acknowledged" by the router
        // worker; the two lines may arrive in either order, so collect both order-independently.
        let lines = [
            wait_for_any_line(&menu_log_rx),
            wait_for_any_line(&menu_log_rx),
        ];
        assert!(lines.iter().any(|l| l.contains("shown")), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("acknowledged")),
            "{lines:?}"
        );
        // "acknowledged" was emitted after the worker re-armed the busy gate, so receiving it
        // proves the press completed — no polling window; a regressed fallback dispatch would
        // have set the intake gate synchronously before the line was logged.
        assert!(!router.in_flight());
        assert!(!router.fallback_in_flight());
        assert_eq!(
            authority.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        while menu_log_rx.try_recv().is_ok() {}

        assert!(router.press());
        assert_eq!(
            pf_wire::read_frame(&mut provider).unwrap(),
            SYSTEM_MENU_ACTION_BODY
        );
        pf_wire::write_frame(&mut provider, SYSTEM_MENU_ACK_BODY).unwrap();
        // The provider survived the post-ack "shown": the second press is acknowledged too.
        wait_for_line(&menu_log_rx, "acknowledged");
        assert!(!router.in_flight());
        assert!(!router.fallback_in_flight());
        assert_eq!(
            authority.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let _ = std::fs::remove_file(sock);
    }

    #[test]
    fn hung_provider_falls_back_after_ack_timeout() {
        let sock = scratch("menu-hung-provider");
        let _ = std::fs::remove_file(&sock);
        let authority = UnixListener::bind(&sock).unwrap();
        let (intake, _log) = logging_intake(&sock, IO_TIMEOUT);
        let ack_timeout = Duration::from_millis(50);
        let router = SystemMenuRouter::spawn_with(intake, ack_timeout, Box::new(|_| {})).unwrap();
        let _hung_provider = register_test_provider(&router);

        // The clock starts BEFORE the press: the fallback can only be dispatched after the
        // acknowledgement wait expired, and [press, connection accepted] is a superset of that
        // wait, so the lower bound cannot false-fail under load — it is the property itself
        // (no early fallback), not a scheduling window.
        let started = Instant::now();
        assert!(router.press());

        let body = accept_safe_return(&authority, Duration::from_secs(10));
        assert_eq!(body, SAFE_RETURN_BODY);
        let elapsed = started.elapsed();
        assert!(
            elapsed >= ack_timeout,
            "fallback fired too early: {elapsed:?}"
        );
        let _ = std::fs::remove_file(sock);
    }

    #[test]
    fn invalid_provider_ack_falls_back_and_unregisters_provider() {
        let sock = scratch("menu-invalid-provider");
        let _ = std::fs::remove_file(&sock);
        let authority = UnixListener::bind(&sock).unwrap();
        let (intake, intake_log) = logging_intake(&sock, IO_TIMEOUT);
        // A deliberately huge ack deadline creates a deterministic separation instead of a
        // timing window: an unregistered provider is never consulted, so the second press's
        // fallback must arrive promptly. A regression that kept the invalid provider
        // registered would first wait out this 30 s deadline, and the 10 s bounded accept
        // below would fail.
        let (router, router_log) = logging_router(intake, Duration::from_secs(30));
        let mut provider = register_test_provider(&router);

        assert!(router.press());
        assert_eq!(
            pf_wire::read_frame(&mut provider).unwrap(),
            SYSTEM_MENU_ACTION_BODY
        );
        pf_wire::write_frame(
            &mut provider,
            br#"{"version":1,"type":"ack","action":"not_system_menu"}"#,
        )
        .unwrap();

        let body = accept_safe_return(&authority, Duration::from_secs(10));
        assert_eq!(body, SAFE_RETURN_BODY);
        // Both outcome lines are emitted after their worker re-armed its busy gate, and the
        // router barrier proves the Press command was fully processed — so the flag asserts
        // below are direct, with no polling window.
        wait_for_line(&router_log, "fallback to safe_return");
        wait_for_line(&intake_log, "safe_return");
        router.sync().unwrap();
        assert!(!router.in_flight());
        assert!(!router.fallback_in_flight());

        assert!(router.press());
        let second_body = accept_safe_return(&authority, Duration::from_secs(10));
        assert_eq!(second_body, SAFE_RETURN_BODY);
        let _ = std::fs::remove_file(sock);
    }

    #[test]
    fn menu_return_item_uses_the_same_safe_return_path() {
        let sock = scratch("menu-return-item");
        let _ = std::fs::remove_file(&sock);
        let authority = UnixListener::bind(&sock).unwrap();
        let (intake, _log) = logging_intake(&sock, IO_TIMEOUT);
        let router =
            SystemMenuRouter::spawn_with(intake, Duration::from_millis(50), Box::new(|_| {}))
                .unwrap();

        let (mut menu, broker) = UnixStream::pair().unwrap();
        let routed = router.clone();
        let trusted_uid = unsafe { libc::geteuid() };
        let server =
            std::thread::spawn(move || serve_system_menu_provider(broker, &routed, trusted_uid));
        pf_wire::write_frame(&mut menu, SYSTEM_MENU_RETURN_BODY).unwrap();
        assert_eq!(
            pf_wire::read_frame(&mut menu).unwrap(),
            SYSTEM_MENU_RETURNED_BODY
        );
        server.join().unwrap().unwrap();

        let body = accept_safe_return(&authority, Duration::from_secs(10));
        assert_eq!(body, SAFE_RETURN_BODY);
        let _ = std::fs::remove_file(sock);
    }

    #[test]
    fn provider_endpoint_rejects_untrusted_uid_and_unsupported_major() {
        let sock = scratch("menu-provider-rejections");
        let _ = std::fs::remove_file(&sock);
        let (intake, _log) = logging_intake(&sock, IO_TIMEOUT);
        let router =
            SystemMenuRouter::spawn_with(intake, Duration::from_millis(50), Box::new(|_| {}))
                .unwrap();
        let (client, server) = UnixStream::pair().unwrap();
        let actual_uid = peer_uid(&client).unwrap();
        let error =
            serve_system_menu_provider(server, &router, actual_uid.wrapping_add(1)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

        let (mut client, server) = UnixStream::pair().unwrap();
        let routed = router.clone();
        let worker =
            std::thread::spawn(move || serve_system_menu_provider(server, &routed, actual_uid));
        pf_wire::write_frame(&mut client, br#"{"version":2,"type":"register"}"#).unwrap();
        let error = worker.join().unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn body_is_the_authority_safe_return_serde_form() {
        let value: serde_json::Value = serde_json::from_slice(SAFE_RETURN_BODY).unwrap();
        assert_eq!(value, serde_json::json!({"method": "safe_return"}));
    }

    #[test]
    fn orientation_snapshot_maps_authority_turns_and_rejects_bad_frames() {
        let reply = serde_json::json!({
            "result": "orientation",
            "version": 1,
            "state": {"serial": 7, "whole_body": "right"}
        });
        assert_eq!(
            parse_orientation(&serde_json::to_vec(&reply).unwrap()).unwrap(),
            (7, Posture::WholeBodyRotated(Rotation::Cw90))
        );
        let normal = serde_json::json!({
            "result": "orientation",
            "version": 1,
            "state": {"serial": 8, "whole_body": "normal"}
        });
        assert_eq!(
            parse_orientation(&serde_json::to_vec(&normal).unwrap()).unwrap(),
            (8, Posture::Normal)
        );

        // Negative controls are malformed, unsupported-version, and partial responses. None
        // may be mistaken for a Normal snapshot or replace the last valid posture.
        for bad in [
            br#"{"result":"orientation","version":1,"state":{"serial":9}}"#.as_slice(),
            br#"{"result":"orientation","version":2,"state":{"serial":9,"whole_body":"right"}}"#
                .as_slice(),
            br#"{"result":"orientation","version":1,"state":{"serial":9,"whole_body":"right"}"#
                .as_slice(),
        ] {
            assert!(
                parse_orientation(bad).is_err(),
                "bad frame accepted: {bad:?}"
            );
        }
    }

    #[test]
    fn orientation_request_is_versioned_and_reads_fake_authority_snapshot() {
        let sock = scratch("orientation-rpc");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request: serde_json::Value =
                serde_json::from_slice(&pf_wire::read_frame(&mut stream).unwrap()).unwrap();
            assert_eq!(request["method"], "orientation");
            assert_eq!(request["version"], 1);
            assert_eq!(request["wait_ms"], 1_000);
            pf_wire::write_frame(
                &mut stream,
                br#"{"result":"orientation","version":1,"state":{"serial":12,"whole_body":"left"}}"#,
            )
            .unwrap();
        });
        assert_eq!(
            read_orientation(&sock, Some(11)).unwrap(),
            (12, Posture::WholeBodyRotated(Rotation::Cw270))
        );
        server.join().unwrap();
        let _ = std::fs::remove_file(sock);
    }

    #[test]
    fn absent_orientation_source_logs_named_fallback_and_stops_cleanly() {
        let sock = scratch("orientation-absent");
        let _ = std::fs::remove_file(&sock);
        let (tx, rx) = channel();
        let feed = PostureFeed::spawn_with(
            &sock,
            Box::new(move |line| {
                let _ = tx.send(line.to_owned());
            }),
        )
        .unwrap();
        let line = wait_for_line(&rx, "orientation_source_unavailable");
        assert!(line.contains("fallback=last_valid_or_normal"), "{line}");
        assert_eq!(feed.current(), Posture::Normal);
        drop(feed);
        let _ = std::fs::remove_file(sock);
    }

    #[test]
    fn gate_consumes_only_guide_and_fires_once_per_press_edge() {
        let sock = scratch("gate-edges");
        let _ = std::fs::remove_file(&sock);
        let (intake, _log) = logging_intake(&sock, IO_TIMEOUT);
        let router =
            SystemMenuRouter::spawn_with(intake, Duration::from_millis(50), Box::new(|_| {}))
                .unwrap();
        let mut gate = SystemMenuGate::new(router);
        assert!(!gate.consume_key(0x130, 1), "BTN_SOUTH passes through");
        assert!(gate.consume_key(GUIDE_CODE, 1));
        assert!(gate.down);
        assert!(gate.consume_key(GUIDE_CODE, 2), "autorepeat is consumed");
        assert!(gate.down, "autorepeat keeps the key down");
        assert!(gate.consume_key(GUIDE_CODE, 0));
        assert!(!gate.down);
    }

    #[test]
    fn absent_socket_fails_fast_with_not_found() {
        let sock = scratch("absent-connect");
        let _ = std::fs::remove_file(&sock);
        let started = Instant::now();
        let err = connect_with_timeout(&sock, CONNECT_TIMEOUT).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        // The 1 s ceiling is a hang safety net, not a synchronisation window: `connect(2)`
        // reports the absent socket immediately, so healthy runs are sub-millisecond; only a
        // regression that turns ENOENT into deadline waiting can approach the ceiling.
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// A listening socket with an explicit backlog (std picks its own, possibly SOMAXCONN).
    fn listener_with_backlog(path: &Path, backlog: libc::c_int) -> UnixListener {
        let (addr, len) = sockaddr_un(path).unwrap();
        // SAFETY: plain socket/bind/listen over a valid sockaddr_un; fd ownership moves into the
        // returned listener.
        unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            assert!(fd >= 0);
            assert_eq!(
                libc::bind(fd, (&addr as *const libc::sockaddr_un).cast(), len),
                0
            );
            assert_eq!(libc::listen(fd, backlog), 0);
            UnixListener::from_raw_fd(fd)
        }
    }

    #[test]
    fn full_backlog_connect_is_deadline_bounded() {
        // A listener that never accepts: fill its backlog so a further connect cannot complete.
        let sock = scratch("full-backlog");
        let _ = std::fs::remove_file(&sock);
        let listener = listener_with_backlog(&sock, 0);
        let mut held = Vec::new();
        let err = loop {
            match connect_with_timeout(&sock, Duration::from_millis(50)) {
                Ok(stream) => held.push(stream),
                Err(e) => break e,
            }
            assert!(held.len() < 64, "backlog never filled");
        };
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        let started = Instant::now();
        let err = connect_with_timeout(&sock, CONNECT_TIMEOUT).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        let elapsed = started.elapsed();
        // The bounds ARE the property: connect must wait the full CONNECT_TIMEOUT against a
        // full backlog, then give up. The lower bound cannot false-fail under load (a deadline
        // wait only stretches); the 1 s ceiling is a hang safety net, generous versus the
        // 250 ms deadline — not a synchronisation window.
        assert!(
            elapsed >= CONNECT_TIMEOUT && elapsed < Duration::from_secs(1),
            "connect gave up after {elapsed:?}"
        );
        drop(listener);
        let _ = std::fs::remove_file(sock);
    }

    #[test]
    fn stalled_authority_read_is_bounded_and_logged() {
        let sock = scratch("stalled-read");
        let _ = std::fs::remove_file(&sock);
        // The 100 ms read deadline is the SUBJECT of the test: a stalled authority must be
        // bounded. Waiting for the outcome line is event-driven, not a timing window.
        let _listener = UnixListener::bind(&sock).unwrap(); // bound, never accepts, never answers
        let (intake, log) = logging_intake(&sock, Duration::from_millis(100));
        assert!(intake.request());
        let line = wait_for_line(&log, "failed");
        assert!(line.contains("safe_return"), "{line}");
        assert!(
            line.contains("WouldBlock") || line.contains("TimedOut"),
            "{line}"
        );
        // The outcome line is emitted after the worker re-armed the gate, so receiving it
        // proves the flag is already false — no polling window.
        assert!(
            !intake.in_flight(),
            "a failed round trip must re-arm the gate"
        );
        let _ = std::fs::remove_file(sock);
    }

    #[test]
    fn non_ok_authority_answer_is_reported_as_failure() {
        let sock = scratch("error-answer");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert_eq!(pf_wire::read_frame(&mut stream).unwrap(), SAFE_RETURN_BODY);
            pf_wire::write_frame(&mut stream, br#"{"result":"error","message":"x"}"#).unwrap();
        });
        let err = send_safe_return(&sock, CONNECT_TIMEOUT, IO_TIMEOUT).unwrap_err();
        assert!(err.to_string().contains("authority answered"), "{err}");
        server.join().unwrap();
        let _ = std::fs::remove_file(sock);
    }
}
