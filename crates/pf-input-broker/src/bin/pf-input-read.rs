//! `pf-input-read` — the test consumer for the v0 INPUT broker proof.
//!
//! Reads input events from the broker's re-emit device for a short window and prints them
//! deterministically (`EV type code value`), so the harness can assert the app sees the REMAPPED
//! (canonical) codes. Two acquisition modes prove different things:
//!
//!   pf-input-read --from-broker <sock> [--ms N]   # get the fd via Acquire("input")+SCM_RIGHTS
//!   pf-input-read --node <eventN>      [--ms N]   # read the re-emit node directly
//!
//! With `--also-check-source <eventN>` it ALSO opens the grabbed source node and reports how many
//! events it could read there — which MUST be 0 while the broker holds the grab (the enforcement
//! assertion: the app cannot bypass the broker to the raw device).

use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use pf_input_broker::{acquire_input_fd, broker::open_read_fd, read_events_raw};

fn arg(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let ms: u64 = arg(&args, "--ms").and_then(|s| s.parse().ok()).unwrap_or(1500);

    // Acquire the read fd either via the broker (SCM_RIGHTS) or by opening the node directly.
    let read_fd = if let Some(sock) = arg(&args, "--from-broker") {
        let (_resp, fd) = acquire_input_fd(&sock)?;
        eprintln!("pf-input-read: acquired input fd via Acquire(\"input\") + SCM_RIGHTS from {sock}");
        fd
    } else if let Some(node) = arg(&args, "--node") {
        eprintln!("pf-input-read: reading re-emit node {node} directly");
        open_read_fd(&node)?
    } else {
        return Err("need --from-broker <sock> or --node <eventN>".into());
    };

    let mut buf: [libc::input_event; 64] = unsafe { std::mem::zeroed() };
    let deadline = Instant::now() + Duration::from_millis(ms);
    let mut count = 0usize;
    while Instant::now() < deadline {
        let n = read_events_raw(read_fd.as_raw_fd(), &mut buf)?;
        for ev in &buf[..n] {
            // Skip SYN in the printed stream (report boundaries are not interesting to assert).
            if ev.type_ == pf_input_broker::ioc::EV_SYN {
                continue;
            }
            println!("EV {} {} {}", ev.type_, ev.code, ev.value);
            count += 1;
        }
        if n == 0 {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    eprintln!("pf-input-read: read {count} events from the re-emit device");

    // Enforcement check: the grabbed source must be SILENT to anyone but the broker.
    if let Some(src) = arg(&args, "--also-check-source") {
        let src_fd = open_read_fd(&src)?;
        let mut src_seen = 0usize;
        let until = Instant::now() + Duration::from_millis(300);
        while Instant::now() < until {
            let n = read_events_raw(src_fd.as_raw_fd(), &mut buf)?;
            src_seen += buf[..n].iter().filter(|e| e.type_ != pf_input_broker::ioc::EV_SYN).count();
            std::thread::sleep(Duration::from_millis(10));
        }
        println!("SOURCE_EVENTS {src_seen}");
        eprintln!("pf-input-read: grabbed source delivered {src_seen} events to us (must be 0)");
    }
    Ok(())
}
