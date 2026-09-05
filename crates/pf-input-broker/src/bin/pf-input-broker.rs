//! `pf-input-broker` — the v0 INPUT broker daemon (`tsp-e1b.6`).
//!
//! Grabs the real evdev source, re-emits a descriptor-canonicalized stream via a uinput device,
//! and hands the re-emit read fd to a consumer over `Acquire("input")` (`SCM_RIGHTS`). The fd is
//! the input hot path (SPIKE-1 / `.1`); PFW1 carries only its acquisition (wire §4.1).
//!
//! Usage:
//!   pf-input-broker --descriptor <caps.toml> [--source <event-node>] [--acquire-sock <path>]
//!
//! `--no-grab` is the R-C blessed-binary path (Steam Link): re-emit + hand the fd WITHOUT the
//! exclusive grab (so a `uinput`-producing consumer is not broken).

use std::os::raw::c_int;
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, Ordering};

use pf_input_broker::{handle_acquire, InputBroker};
use pocketforge::Descriptor;

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
    let desc_path = arg(&args, "--descriptor")
        .or_else(|| std::env::var("PF_DESCRIPTOR").ok())
        .ok_or("need --descriptor <caps.toml> (or PF_DESCRIPTOR)")?;
    let acquire_sock = arg(&args, "--acquire-sock");
    let grab = !args.iter().any(|a| a == "--no-grab");

    let descriptor = Descriptor::load(&desc_path)?;
    let source = match arg(&args, "--source") {
        Some(path) => std::path::PathBuf::from(path),
        None => discover_with_timeout(&descriptor)?,
    };
    let mut broker = InputBroker::start_with(&source, &descriptor, grab)?;
    let node = broker
        .node_path()
        .ok_or("could not resolve the re-emit event node")?;

    // SAFETY: installing simple signal handlers that only set an atomic flag. Cast via a fn
    // pointer (not a fn item) so the conversion to sighandler_t is explicit, not a numeric cast.
    let handler = on_signal as extern "C" fn(c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }

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
    // Bind before readiness: READY means both the event node and acquisition endpoint exist.
    let listener = if let Some(sock) = acquire_sock.as_deref() {
        let _ = std::fs::remove_file(sock);
        let listener = UnixListener::bind(sock)?;
        listener.set_nonblocking(true)?;
        Some(listener)
    } else {
        None
    };
    notify_ready()?;
    println!("ready");
    use std::io::Write;
    std::io::stdout().flush().ok();

    // Pump in the background and propagate every failure to the process exit status.
    let (pump_tx, pump_rx) = std::sync::mpsc::sync_channel(1);
    let pump = std::thread::spawn(move || {
        let _ = pump_tx.send(broker.run(&STOP));
    });

    let result = supervise(listener.as_ref(), &node, &pump_rx, &STOP);
    STOP.store(true, Ordering::Release);
    let _ = pump.join();
    result.map_err(Into::into)
}

fn supervise(
    listener: Option<&UnixListener>,
    node: &str,
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
                    // A silent acquisition client must never hold up pump-failure or signal
                    // observation. The handler also carries a finite I/O deadline.
                    std::thread::spawn(move || {
                        let _ = handle_acquire(stream, &node);
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn discover_with_timeout(descriptor: &Descriptor) -> std::io::Result<std::path::PathBuf> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match InputBroker::discover(descriptor) {
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
            supervise(Some(&listener), "/unused", &rx, &stop)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::BrokenPipe
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn stalled_client_does_not_delay_stop_cleanup() {
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
        supervise(Some(&listener), "/unused", &rx, &stop).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        std::fs::remove_file(path).unwrap();
    }
}
