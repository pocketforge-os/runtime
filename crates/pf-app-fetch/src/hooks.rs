//! Test-only inputs. They exist ONLY in debug builds (`cfg(debug_assertions)`): the image
//! ships a `--release` binary, whose `--version` prints `test-hooks=off` and which ignores
//! every `PF_APP_FETCH_TEST_*` variable. The hermetic tests run the debug binary.
//!
//! - `PF_APP_FETCH_TEST_CA=<der>`: the only trusted root (replaces the CA bundle).
//! - `PF_APP_FETCH_TEST_RESOLVE=host=ip:port,...`: name -> socket address (loopback).
//! - `PF_APP_FETCH_TEST_REDIRECTS=from>to,...`: replaces the platform redirect table.
//! - `PF_APP_FETCH_TEST_STOP_AT=<step>` + `PF_APP_FETCH_TEST_STOP_MARKER=<path>`: write the
//!   marker and `raise(SIGSTOP)` at a protocol step, so the harness can SIGKILL there.
//! - `PF_APP_FETCH_TEST_FREE_BYTES=<n>`: the statvfs free-space figure.
//! - `PF_APP_FETCH_TEST_CONFINE=off|best-effort|required` (debug default `off`).

use std::collections::BTreeMap;
use std::net::SocketAddr;

pub const ENABLED: bool = cfg!(debug_assertions);

pub fn var(name: &str) -> Option<String> {
    if ENABLED {
        std::env::var(name).ok()
    } else {
        None
    }
}

pub fn resolve_overrides() -> BTreeMap<String, SocketAddr> {
    let mut out = BTreeMap::new();
    for pair in var("PF_APP_FETCH_TEST_RESOLVE")
        .unwrap_or_default()
        .split(',')
        .filter(|p| !p.is_empty())
    {
        if let Some((host, addr)) = pair.split_once('=') {
            if let Ok(addr) = addr.parse() {
                out.insert(host.to_string(), addr);
            }
        }
    }
    out
}

pub fn redirect_table() -> Option<BTreeMap<String, Vec<String>>> {
    let raw = var("PF_APP_FETCH_TEST_REDIRECTS")?;
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for pair in raw.split(',').filter(|p| !p.is_empty()) {
        if let Some((from, to)) = pair.split_once('>') {
            out.entry(from.to_string())
                .or_default()
                .push(to.to_string());
        }
    }
    Some(out)
}

pub fn free_bytes() -> Option<u64> {
    var("PF_APP_FETCH_TEST_FREE_BYTES")?.parse().ok()
}

/// A protocol step the kill tests stop at.
pub fn stop_at(step: &str) {
    if var("PF_APP_FETCH_TEST_STOP_AT").as_deref() != Some(step) {
        return;
    }
    if let Some(marker) = var("PF_APP_FETCH_TEST_STOP_MARKER") {
        let _ = std::fs::write(marker, step);
    }
    // SAFETY: raise(2) with a valid signal number.
    unsafe {
        libc::raise(libc::SIGSTOP);
    }
}
