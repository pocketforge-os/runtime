//! `pf-broker` — the enforcing capability-broker daemon (`tsp-e1b.3`).
//!
//! Loads a device descriptor + an `app.toml`, VALIDATES the `use = [...]` authority graph against
//! the descriptor (refusing to launch on any violation), then serves the enforcing backend over a
//! PFW1 Unix socket with an `SO_PEERCRED` uid check. This is the v0 NON-NAMESPACED reference impl:
//! it proves the authority graph + default-deny + quotas + peer-cred over the socket; real
//! namespace fd-isolation is the substrate-gated leg (owned kernel M2.B-E + paused M1.D).
//!
//! Usage:
//!   pf-broker --socket <path> --descriptor <caps.toml> --manifest <app.toml> [--peer-uid <uid>]
//!   pf-broker --validate-only --descriptor <caps.toml> --manifest <app.toml>   # launch gate only

use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use pocketforge::backends::InProcessBackend;
use pocketforge::{Backend, Descriptor};

use pf_broker::{serve_enforcing_until, AppManifest, EnforcingBackend};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn arg(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let desc_path = arg(&args, "--descriptor")
        .or_else(|| std::env::var("PF_DESCRIPTOR").ok())
        .ok_or("need --descriptor <caps.toml>")?;
    let manifest_path = arg(&args, "--manifest").ok_or("need --manifest <app.toml>")?;
    let validate_only = args.iter().any(|a| a == "--validate-only");

    let descriptor = Descriptor::load(&desc_path)?;
    let manifest = AppManifest::load(&manifest_path)?;

    // LAUNCH GATE: validate the authority graph against the descriptor; refuse to launch on any
    // violation (print every reason, exit non-zero — the supervisor reads this).
    let validated = match manifest.validate(&descriptor) {
        Ok(v) => v,
        Err(violations) => {
            eprintln!("pf-broker: REFUSING to launch '{}' — app.toml violates the authority graph:", manifest.app.id);
            for v in &violations {
                eprintln!("  - {v}");
            }
            std::process::exit(2);
        }
    };
    println!(
        "validated app={} ceiling=[{}]",
        validated.app_id,
        validated.allowed_caps().collect::<Vec<_>>().join(",")
    );
    if validate_only {
        println!("ok (validate-only)");
        return Ok(());
    }

    let socket = arg(&args, "--socket").ok_or("need --socket <path> (or --validate-only)")?;
    let peer_uid = arg(&args, "--peer-uid").and_then(|s| s.parse::<u32>().ok());

    let inner: Arc<dyn Backend> = InProcessBackend::shared(Arc::new(descriptor));
    let enforcing: Arc<dyn Backend> = Arc::new(EnforcingBackend::new(inner, &validated));

    // SAFETY: signal handlers only set an atomic flag.
    let handler = on_signal as extern "C" fn(c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }

    let _ = std::fs::remove_file(&socket);
    let listener = std::os::unix::net::UnixListener::bind(&socket)?;
    eprintln!(
        "pf-broker: serving {socket} (descriptor {desc_path}, app {}) — ENFORCING (default-deny, manifest ceiling, SO_PEERCRED{}) — NON-NAMESPACED reference impl",
        validated.app_id,
        peer_uid.map(|u| format!(", peer-uid={u}")).unwrap_or_default()
    );
    println!("ready");
    use std::io::Write;
    std::io::stdout().flush().ok();

    serve_enforcing_until(listener, enforcing, peer_uid, &STOP)?;
    Ok(())
}
