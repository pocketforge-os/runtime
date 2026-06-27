//! `pf-broker-ref` — the reference PFW1 broker.
//!
//! It binds a Unix socket and serves the v0 [`InProcessBackend`] over [`server::serve`], so a
//! `BrokerClientBackend` (and E6) can run against an out-of-process broker off hardware. This
//! is the cooperative loopback that proves the backend-swap seam; the ENFORCING daemon
//! (default-deny vs. hostile, `SO_PEERCRED`, quotas, namespace fd-routing) is `tsp-e1b.3`.
//!
//! Usage:
//!   pf-broker-ref <socket-path> [<capabilities.toml>]
//!   # descriptor also accepted via PF_DESCRIPTOR

use std::os::unix::net::UnixListener;
use std::sync::Arc;

use pocketforge::backends::InProcessBackend;
use pocketforge::{server, Backend, Descriptor};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let sock = args
        .next()
        .ok_or("usage: pf-broker-ref <socket-path> [<capabilities.toml>]")?;
    let desc_path = args
        .next()
        .or_else(|| std::env::var("PF_DESCRIPTOR").ok())
        .ok_or("need a descriptor: pass it as arg 2 or set PF_DESCRIPTOR")?;

    let descriptor = Arc::new(Descriptor::load(&desc_path)?);
    let backend: Arc<dyn Backend> = InProcessBackend::shared(descriptor);

    // Fresh socket each run (the supervisor owns the canonical path in production).
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;
    eprintln!("pf-broker-ref: serving {sock} (descriptor {desc_path}) — cooperative reference, NOT enforcing");
    server::serve(listener, backend)?;
    Ok(())
}
