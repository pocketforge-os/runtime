//! The broker as the **protected SafeReturn intake** (`tsp-f3fm.202.1`, option (i)).
//!
//! While an app is foreground the broker holds the pad's `EVIOCGRAB`, so it is the one process
//! that can see guide/Menu (`BTN_MODE`, `0x13c`) without trusting the app. With
//! `--safe-return-sock PATH` the broker drops every guide press and release from the re-emit
//! stream, so the app never observes it. On each guide PRESS edge it asks the session authority
//! to return to the launcher by sending one pf-wire frame carrying `{"method":"safe_return"}`.
//! That body is the authority's `RpcRequest::SafeReturn` serde form (`tag = "method"`,
//! `snake_case`).
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
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The canonical guide/Menu key (`BTN_MODE`) the broker reserves for SafeReturn.
pub const GUIDE_CODE: u16 = 0x13c;

/// Connect deadline for the authority socket.
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);

/// Read (and write) deadline for one SafeReturn round trip. It covers an authority that is busy
/// running `systemctl stop` for the app's `TimeoutStopSec`.
pub const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// The pf-wire frame body: exactly `serde_json::to_vec(&RpcRequest::SafeReturn)`.
pub const SAFE_RETURN_BODY: &[u8] = br#"{"method":"safe_return"}"#;

/// Where the worker reports each round trip's outcome, one line per request.
pub type LogSink = Box<dyn Fn(&str) + Send + 'static>;

/// Handle to the SafeReturn worker. Cheap to call from the input pump: [`request`](Self::request)
/// never blocks and never performs I/O.
pub struct SafeReturnIntake {
    tx: SyncSender<()>,
    busy: Arc<AtomicBool>,
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
                    log(&outcome_line(&sock, &outcome));
                    worker_busy.store(false, Ordering::Release);
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

/// Tracks the guide key's state and turns its press edges into SafeReturn requests. Every guide
/// transition is consumed, so the app never observes it.
pub struct SafeReturnGate {
    intake: SafeReturnIntake,
    down: bool,
}

impl SafeReturnGate {
    pub fn new(intake: SafeReturnIntake) -> SafeReturnGate {
        SafeReturnGate {
            intake,
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
            self.intake.request();
        }
        self.down = down;
    }

    pub fn intake(&self) -> &SafeReturnIntake {
        &self.intake
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

    #[test]
    fn body_is_the_authority_safe_return_serde_form() {
        let value: serde_json::Value = serde_json::from_slice(SAFE_RETURN_BODY).unwrap();
        assert_eq!(value, serde_json::json!({"method": "safe_return"}));
    }

    #[test]
    fn gate_consumes_only_guide_and_fires_once_per_press_edge() {
        let sock = scratch("gate-edges");
        let _ = std::fs::remove_file(&sock);
        let (intake, _log) = logging_intake(&sock, IO_TIMEOUT);
        let mut gate = SafeReturnGate::new(intake);
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
        let _listener = UnixListener::bind(&sock).unwrap(); // bound, never accepts, never answers
        let (intake, log) = logging_intake(&sock, Duration::from_millis(100));
        assert!(intake.request());
        let line = log.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            line.contains("safe_return") && line.contains("failed"),
            "{line}"
        );
        assert!(
            line.contains("WouldBlock") || line.contains("TimedOut"),
            "{line}"
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        while intake.in_flight() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
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
