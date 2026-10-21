//! `pf-input-broker` — the v0 INPUT broker daemon (`tsp-e1b.6`).
//!
//! Grabs the real evdev source, re-emits a descriptor-canonicalized stream via a uinput device,
//! and hands the re-emit read fd to a consumer over `Acquire("input")` (`SCM_RIGHTS`). The fd is
//! the input hot path (SPIKE-1 / `.1`); PFW1 carries only its acquisition (wire §4.1).
//!
//! Usage:
//!   pf-input-broker --descriptor <caps.toml> [--source <event-node>] [--acquire-sock <path>]
//!                   [--safe-return-sock <authority.sock>]
//!                   [--system-menu-sock <provider.sock>]
//!                   [--volume-keys [--volume-toast-sock <osd.sock>]]
//!                   [--allow-unidentified-pad]
//!   pf-input-broker --descriptor <caps.toml>
//!                   --test-event-fd <inherited-fd> --test-record-fd <inherited-fd>
//!
//! Each `--acquire-sock` connection is a persistent PFW1 session: repeated `Acquire("input")`
//! (each answered with a fresh re-emit fd), read-only preference operations (prefsd via
//! `$PF_PREFSD_SOCK`), and a
//! typed `Unsupported` for everything else.
//!
//! `--safe-return-sock` makes guide/`BTN_MODE` a protected System Menu action. A same-uid trusted
//! provider can register at `--system-menu-sock`; it receives versioned `system_menu` actions and
//! must acknowledge within 250 ms. Missing, invalid, disconnected, or hung providers fall back to
//! the existing `safe_return` authority RPC. A provider's versioned `shown` message is logged but
//! has no deadline policy in v1. The input pump never waits on either process.
//!
//! `--no-grab` is the R-C blessed-binary path (Steam Link): re-emit + hand the fd WITHOUT the
//! exclusive grab (so a `uinput`-producing consumer is not broken).

use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::raw::c_int;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use pf_input_broker::{
    is_private_stream_eof, serve_client, serve_system_menu_provider, FdEventSink, FdEventSource,
    InputBroker, PostureFeed, RunConfig, SafeReturnIntake, SourceIdentity, SystemMenuRouter,
};
use pocketforge::backends::InProcessBackend;
use pocketforge::{Backend, Descriptor};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn arg(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = test_io_mode(&args)?;
    dispatch_mode(
        mode,
        |test_io| run_private(&args, test_io),
        || run_production(&args),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TestIo {
    event_fd: RawFd,
    record_fd: RawFd,
}

fn fd_arg(args: &[String], flag: &str) -> Result<Option<RawFd>, Box<dyn std::error::Error>> {
    let Some(index) = args.iter().position(|arg| arg == flag) else {
        return Ok(None);
    };
    let value = args
        .get(index + 1)
        .ok_or_else(|| format!("{flag} requires a non-negative inherited fd"))?;
    let fd: RawFd = value
        .parse()
        .map_err(|_| format!("{flag} requires a non-negative inherited fd"))?;
    if fd < 0 {
        return Err(format!("{flag} requires a non-negative inherited fd").into());
    }
    Ok(Some(fd))
}

fn test_io_mode(args: &[String]) -> Result<Option<TestIo>, Box<dyn std::error::Error>> {
    let event_fd = fd_arg(args, "--test-event-fd")?;
    let record_fd = fd_arg(args, "--test-record-fd")?;
    let mode = match (event_fd, record_fd) {
        (None, None) => return Ok(None),
        (Some(event_fd), Some(record_fd)) => TestIo {
            event_fd,
            record_fd,
        },
        _ => return Err("--test-event-fd and --test-record-fd must be supplied together".into()),
    };
    if mode.event_fd == mode.record_fd {
        return Err("test event and record fds must be distinct".into());
    }
    for flag in [
        "--source",
        "--no-grab",
        "--acquire-sock",
        "--safe-return-sock",
        "--system-menu-sock",
        "--allow-unidentified-pad",
        "--volume-keys",
        "--volume-toast-sock",
    ] {
        if args.iter().any(|arg| arg == flag) {
            return Err(format!("{flag} is unavailable with private test fds").into());
        }
    }
    Ok(Some(mode))
}

fn dispatch_mode<T>(
    mode: Option<TestIo>,
    private: impl FnOnce(TestIo) -> T,
    production: impl FnOnce() -> T,
) -> T {
    match mode {
        Some(test_io) => private(test_io),
        None => production(),
    }
}

fn descriptor_path(args: &[String]) -> Result<String, Box<dyn std::error::Error>> {
    arg(args, "--descriptor")
        .or_else(|| std::env::var("PF_DESCRIPTOR").ok())
        .ok_or_else(|| "need --descriptor <caps.toml> (or PF_DESCRIPTOR)".into())
}

fn take_inherited_fd(fd: RawFd) -> std::io::Result<OwnedFd> {
    // SAFETY: F_GETFD validates that the inherited descriptor is open without changing it.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: this mode takes ownership of each distinct inherited descriptor exactly once.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn install_signal_handlers() {
    // SAFETY: the handlers only set an atomic flag. Cast via a fn pointer so the conversion to
    // sighandler_t is explicit.
    let handler = on_signal as extern "C" fn(c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
}

fn run_private(args: &[String], test_io: TestIo) -> Result<(), Box<dyn std::error::Error>> {
    let descriptor = Descriptor::load(descriptor_path(args)?)?;
    let mut source = FdEventSource::from_owned_fd(take_inherited_fd(test_io.event_fd)?);
    let mut sink = FdEventSink::from_owned_fd(take_inherited_fd(test_io.record_fd)?);
    install_signal_handlers();
    println!("ready");
    use std::io::Write;
    std::io::stdout().flush()?;
    match InputBroker::run_with_io(&mut source, &mut sink, RunConfig::new(&descriptor, &STOP)) {
        // A finite private stream ends by closing its writer. A partial record or syscall error
        // remains an error and exits non-zero.
        Err(error) if is_private_stream_eof(&error) => Ok(()),
        result => result.map_err(Into::into),
    }
}

/// Volume keys are system keys: handled here, never routed to apps (tsp-ihne1.63). The thread
/// owns its own non-grabbing fd on the LRADC key node and writes through pf-prefsd as this unit.
fn spawn_volume_keys(toast_sock: Option<String>) {
    use pf_volume_keys::host::{discover_keys, run, EvdevKeys, LazyJack, SocketSink, ToastSink};
    use pf_volume_keys::{PrefsdStore, VolumeKeys};
    struct Null;
    impl ToastSink for Null {
        fn show(&mut self, _: &pf_volume_keys::ToastModel) {}
    }
    std::thread::spawn(move || {
        let sock = std::env::var("PF_PREFSD_SOCK")
            .unwrap_or_else(|_| "/run/pocketforge/prefsd.sock".into());
        let started = std::time::Instant::now();
        while !STOP.load(Ordering::Relaxed) {
            let source = discover_keys("/sys/class/input".as_ref(), "/dev/input".as_ref())
                .and_then(|n| EvdevKeys::open(&n));
            let mut source = match source {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("pf-input-broker: volume keys unavailable, retrying: {e}");
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    continue;
                }
            };
            let mut volume = VolumeKeys::new(
                PrefsdStore(pf_prefsd::Client::new(&sock)),
                LazyJack::new("/sys/class/input", "/dev/input"),
            );
            let result = match toast_sock.as_deref().map(SocketSink::new) {
                Some(Ok(mut sink)) => run(
                    &mut volume,
                    &mut source,
                    &mut sink,
                    || started.elapsed(),
                    &STOP,
                ),
                _ => run(
                    &mut volume,
                    &mut source,
                    &mut Null,
                    || started.elapsed(),
                    &STOP,
                ),
            };
            if let Err(e) = result {
                eprintln!("pf-input-broker: volume keys stopped, retrying: {e}");
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    });
}

fn run_production(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let desc_path = descriptor_path(args)?;
    let acquire_sock = arg(args, "--acquire-sock");
    let safe_return_sock = arg(args, "--safe-return-sock");
    let system_menu_sock = arg(args, "--system-menu-sock");
    if system_menu_sock.is_some() && safe_return_sock.is_none() {
        return Err("--system-menu-sock requires --safe-return-sock".into());
    }
    let grab = !args.iter().any(|a| a == "--no-grab");
    // Identity is fail-closed by default: the source must be the decoder's built-in controller
    // (virtual path + decoder phys + trusted record), not merely descriptor-compatible.
    let identity = source_identity(args);
    if let Some(warning) = identity_warning(&identity) {
        eprintln!("{warning}");
    }

    let descriptor = Descriptor::load(&desc_path)?;
    let source = match arg(args, "--source") {
        Some(path) => std::path::PathBuf::from(path),
        None => discover_with_timeout(&descriptor, &identity)?,
    };
    let mut broker = InputBroker::start_checked(&source, &descriptor, grab, &identity)?;
    if let Some(sock) = safe_return_sock.as_deref() {
        broker = broker.with_posture_feed(PostureFeed::spawn(sock)?);
    } else {
        eprintln!(
            "pf-input-broker: orientation_source_unavailable source=none reason=authority_socket_missing fallback=normal"
        );
    }
    let system_menu_router = safe_return_sock
        .as_deref()
        .map(SafeReturnIntake::spawn)
        .transpose()?
        .map(SystemMenuRouter::spawn)
        .transpose()?;
    if let Some(router) = system_menu_router.as_ref() {
        broker = broker.with_system_menu(router.clone());
    }
    // GetAppearance fallback when $PF_PREFSD_SOCK is unset: the store-less in-process default.
    let backend: Arc<dyn Backend> = Arc::new(InProcessBackend::new(Arc::new(descriptor)));
    let node = broker
        .node_path()
        .ok_or("could not resolve the re-emit event node")?;

    install_signal_handlers();

    let src_name = broker.source_name().unwrap_or_default();
    eprintln!(
        "pf-input-broker: source={} ({src_name}) grab={grab} re-emit={node}{}",
        source.display(),
        if grab {
            "  [ENFORCING: source grabbed]"
        } else {
            "  [blessed no-grab]"
        }
    );
    println!("node={node}");
    if let Some(sock) = acquire_sock.as_deref() {
        println!("acquire-sock={sock}");
    }
    if let Some(sock) = safe_return_sock.as_deref() {
        eprintln!("pf-input-broker: System Menu fallback -> {sock} (guide withheld from apps)");
    }
    // Bind before readiness: READY means the event node and configured endpoints all exist.
    let listener = if let Some(sock) = acquire_sock.as_deref() {
        let _ = std::fs::remove_file(sock);
        let listener = UnixListener::bind(sock)?;
        listener.set_nonblocking(true)?;
        Some(listener)
    } else {
        None
    };
    let system_menu_listener = if let Some(sock) = system_menu_sock.as_deref() {
        let _ = std::fs::remove_file(sock);
        let listener = UnixListener::bind(sock)?;
        std::fs::set_permissions(sock, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        println!("system-menu-sock={sock}");
        Some(listener)
    } else {
        None
    };
    if args.iter().any(|a| a == "--volume-keys") {
        spawn_volume_keys(arg(args, "--volume-toast-sock"));
    }
    notify_ready()?;
    println!("ready");
    use std::io::Write;
    std::io::stdout().flush().ok();

    // Pump in the background and propagate every failure to the process exit status.
    let (pump_tx, pump_rx) = std::sync::mpsc::sync_channel(1);
    let pump = std::thread::spawn(move || {
        let _ = pump_tx.send(broker.run(&STOP));
    });

    let result = supervise(
        listener.as_ref(),
        system_menu_listener.as_ref(),
        system_menu_router.as_ref(),
        &node,
        &backend,
        &pump_rx,
        &STOP,
    );
    STOP.store(true, Ordering::Release);
    let _ = pump.join();
    result.map_err(Into::into)
}

fn supervise(
    listener: Option<&UnixListener>,
    system_menu_listener: Option<&UnixListener>,
    system_menu_router: Option<&SystemMenuRouter>,
    node: &str,
    backend: &Arc<dyn Backend>,
    pump_rx: &std::sync::mpsc::Receiver<std::io::Result<()>>,
    stop: &AtomicBool,
) -> std::io::Result<()> {
    loop {
        if let Ok(result) = pump_rx.try_recv() {
            return result;
        }
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Some(listener) = listener {
            match listener.accept() {
                Ok((stream, _)) => {
                    let node = node.to_owned();
                    let backend = backend.clone();
                    // A persistent (or silent) client must never hold up pump-failure or signal
                    // observation: each session runs on its own thread, and every request
                    // carries a finite I/O deadline once it starts.
                    std::thread::spawn(move || {
                        let _ = serve_client(stream, &node, &*backend);
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        if let (Some(listener), Some(router)) = (system_menu_listener, system_menu_router) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let router = router.clone();
                    let trusted_uid = unsafe { libc::geteuid() };
                    std::thread::spawn(move || {
                        if let Err(error) = serve_system_menu_provider(stream, &router, trusted_uid)
                        {
                            eprintln!("pf-input-broker: rejected system-menu provider: {error}");
                        }
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn source_identity(args: &[String]) -> SourceIdentity {
    if args.iter().any(|a| a == "--allow-unidentified-pad") {
        SourceIdentity::allow_unidentified()
    } else {
        SourceIdentity::system()
    }
}

fn identity_warning(identity: &SourceIdentity) -> Option<&'static str> {
    (!identity.is_enforcing()).then_some(
        "pf-input-broker: WARNING: --allow-unidentified-pad disables built-in controller identity checks; use only for desktop simulation and tests",
    )
}

fn discover_with_timeout(
    descriptor: &Descriptor,
    identity: &SourceIdentity,
) -> std::io::Result<std::path::PathBuf> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match InputBroker::discover_checked(descriptor, identity) {
            Ok(path) => return Ok(path),
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(250))
            }
            Err(e) => return Err(e),
        }
    }
}

fn notify_ready() -> std::io::Result<()> {
    let Ok(socket) = std::env::var("NOTIFY_SOCKET") else {
        return Ok(());
    };
    send_ready(&socket)
}

fn send_ready(socket: &str) -> std::io::Result<()> {
    let bytes = socket.as_bytes();
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let (offset, path): (usize, &[u8]) = if bytes.first() == Some(&b'@') {
        (1, &bytes[1..])
    } else {
        (0, bytes)
    };
    if path.len() + offset >= addr.sun_path.len() {
        unsafe { libc::close(fd) };
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "NOTIFY_SOCKET path too long",
        ));
    }
    for (i, byte) in path.iter().enumerate() {
        addr.sun_path[i + offset] = *byte as libc::c_char;
    }
    let base = std::mem::size_of::<libc::sa_family_t>();
    let len = base + offset + path.len() + usize::from(offset == 0);
    let msg = b"READY=1";
    let rc = unsafe {
        libc::sendto(
            fd,
            msg.as_ptr().cast(),
            msg.len(),
            0,
            (&addr as *const libc::sockaddr_un).cast(),
            len as libc::socklen_t,
        )
    };
    let err = if rc < 0 {
        Some(std::io::Error::last_os_error())
    } else {
        None
    };
    unsafe { libc::close(fd) };
    err.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// Slice of this file's source between `start` and the next top-level `fn `.
    fn body_of(start: &str) -> &'static str {
        const SOURCE: &str = include_str!("pf-input-broker.rs");
        let from = SOURCE.find(start).expect("function present");
        let rest = &SOURCE[from + start.len()..];
        &SOURCE[from..from + start.len() + rest.find("\nfn ").unwrap_or(rest.len())]
    }

    #[test]
    fn volume_keys_start_in_the_production_path_and_never_in_private_test_io() {
        // The private-fd mode rejects --volume-keys (test_io_mode), so a spawn there is dead code
        // and production would silently never start the handler.
        assert!(body_of("fn run_production(").contains("spawn_volume_keys("));
        assert!(!body_of("fn run_private(").contains("spawn_volume_keys("));
        // Negative control for the helper itself: it must see a call that exists.
        assert!(body_of("fn run_production(").contains("notify_ready()"));
    }

    #[test]
    fn private_fd_flags_bypass_counted_production_openers() {
        let args = vec![
            "--test-event-fd".to_owned(),
            "7".to_owned(),
            "--test-record-fd".to_owned(),
            "8".to_owned(),
        ];
        let mode = test_io_mode(&args).unwrap();
        let evdev_opens = Cell::new(0);
        let uinput_opens = Cell::new(0);

        let selected = dispatch_mode(
            mode,
            |_| "private",
            || {
                evdev_opens.set(evdev_opens.get() + 1);
                uinput_opens.set(uinput_opens.get() + 1);
                "production"
            },
        );

        assert_eq!(selected, "private");
        assert_eq!(evdev_opens.get(), 0);
        assert_eq!(uinput_opens.get(), 0);
    }

    #[test]
    fn private_fd_flags_are_paired_and_reject_production_options() {
        assert!(test_io_mode(&["--test-event-fd".into(), "7".into()]).is_err());
        assert!(test_io_mode(&[
            "--test-event-fd".into(),
            "7".into(),
            "--test-record-fd".into(),
            "8".into(),
            "--source".into(),
            "unused".into(),
        ])
        .is_err());
        assert_eq!(test_io_mode(&[]).unwrap(), None);
    }

    #[test]
    fn built_in_pad_identity_is_default_and_opt_out_is_explicit_and_loud() {
        let default = source_identity(&[]);
        assert!(default.is_enforcing());
        assert_eq!(identity_warning(&default), None);

        let opt_out = source_identity(&["--allow-unidentified-pad".to_owned()]);
        assert!(!opt_out.is_enforcing());
        assert_eq!(
            identity_warning(&opt_out),
            Some(
                "pf-input-broker: WARNING: --allow-unidentified-pad disables built-in controller identity checks; use only for desktop simulation and tests"
            )
        );

        // The old opt-in cannot accidentally become an opt-out.
        assert!(source_identity(&["--require-builtin".to_owned()]).is_enforcing());
    }

    fn test_backend() -> Arc<dyn Backend> {
        Arc::new(InProcessBackend::new(Arc::new(
            pocketforge::test_support::gnss_descriptor(),
        )))
    }

    #[test]
    fn explicit_source_override_is_accepted() {
        let args = vec!["--source".to_owned(), "/dev/input/event-test".to_owned()];
        assert_eq!(
            arg(&args, "--source").as_deref(),
            Some("/dev/input/event-test")
        );
    }

    #[test]
    fn readiness_notification_is_ready_one_datagram() {
        let path =
            std::env::temp_dir().join(format!("pf-input-broker-notify-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let receiver = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
        send_ready(path.to_str().unwrap()).unwrap();
        let mut buf = [0u8; 32];
        let n = receiver.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"READY=1");
        drop(receiver);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn stalled_client_does_not_hide_pump_failure() {
        // The 1 s ceiling is a liveness safety net, not a synchronisation window: supervise
        // polls its command channel every 50 ms, so the pump failure surfaces within one
        // tick of a healthy run; only a regression that blocks on the silent client can
        // reach the ceiling.
        let path =
            std::env::temp_dir().join(format!("pf-input-broker-acquire-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let _silent_client = std::os::unix::net::UnixStream::connect(&path).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "pump failed",
        )))
        .unwrap();
        let stop = AtomicBool::new(false);
        let started = std::time::Instant::now();
        assert_eq!(
            supervise(
                Some(&listener),
                None,
                None,
                "/unused",
                &test_backend(),
                &rx,
                &stop,
            )
            .unwrap_err()
            .kind(),
            std::io::ErrorKind::BrokenPipe
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn stalled_client_does_not_delay_stop_cleanup() {
        // The 1 s ceiling is a liveness safety net, not a synchronisation window: supervise
        // observes the pre-set stop flag on its first 50 ms poll tick, so a healthy run
        // returns immediately; only a regression that blocks on the silent client can reach
        // the ceiling.
        let path = std::env::temp_dir().join(format!(
            "pf-input-broker-acquire-stop-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let _silent_client = std::os::unix::net::UnixStream::connect(&path).unwrap();
        let (_tx, rx) = std::sync::mpsc::channel();
        let stop = AtomicBool::new(true);
        let started = std::time::Instant::now();
        supervise(
            Some(&listener),
            None,
            None,
            "/unused",
            &test_backend(),
            &rx,
            &stop,
        )
        .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        std::fs::remove_file(path).unwrap();
    }
}
