//! `pf-input-broker` — the v0 INPUT broker daemon (`tsp-e1b.6`).
//!
//! Grabs the real evdev source, re-emits a descriptor-canonicalized stream via a uinput device,
//! and hands the re-emit read fd to a consumer over `Acquire("input")` (`SCM_RIGHTS`). The fd is
//! the input hot path (SPIKE-1 / `.1`); PFW1 carries only its acquisition (wire §4.1).
//!
//! Usage:
//!   pf-input-broker --source <eventN> --descriptor <caps.toml> [--acquire-sock <path>] [--no-grab]
//!
//! `--no-grab` is the R-C blessed-binary path (Steam Link): re-emit + hand the fd WITHOUT the
//! exclusive grab (so a `uinput`-producing consumer is not broken).

use std::os::raw::c_int;
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, Ordering};

use pf_input_broker::{serve_acquire, InputBroker};
use pocketforge::Descriptor;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn arg(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let source = arg(&args, "--source")
        .ok_or("usage: pf-input-broker --source <eventN> --descriptor <caps.toml> [--acquire-sock <path>] [--no-grab]")?;
    let desc_path = arg(&args, "--descriptor")
        .or_else(|| std::env::var("PF_DESCRIPTOR").ok())
        .ok_or("need --descriptor <caps.toml> (or PF_DESCRIPTOR)")?;
    let acquire_sock = arg(&args, "--acquire-sock");
    let grab = !args.iter().any(|a| a == "--no-grab");

    let descriptor = Descriptor::load(&desc_path)?;
    let mut broker = InputBroker::start_with(&source, &descriptor, grab)?;
    let node = broker.node_path().ok_or("could not resolve the re-emit event node")?;

    // SAFETY: installing simple signal handlers that only set an atomic flag. Cast via a fn
    // pointer (not a fn item) so the conversion to sighandler_t is explicit, not a numeric cast.
    let handler = on_signal as extern "C" fn(c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }

    let src_name = broker.source_name().unwrap_or_default();
    eprintln!(
        "pf-input-broker: source={source} ({src_name}) grab={grab} re-emit={node}{}",
        if grab { "  [ENFORCING: source grabbed]" } else { "  [blessed no-grab]" }
    );
    println!("node={node}");
    if let Some(sock) = acquire_sock.as_deref() {
        println!("acquire-sock={sock}");
    }
    println!("ready");
    use std::io::Write;
    std::io::stdout().flush().ok();

    // Pump in the background; serve Acquire("input") on the main thread (if a socket was given).
    let pump = std::thread::spawn(move || {
        let _ = broker.run(&STOP);
    });

    if let Some(sock) = acquire_sock {
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock)?;
        serve_acquire(&listener, &node, &STOP)?;
    } else {
        while !STOP.load(Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    let _ = pump.join();
    Ok(())
}
