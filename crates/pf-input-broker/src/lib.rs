//! # `pf-input-broker` — the v0 INPUT broker (`tsp-e1b.6`)
//!
//! The ONE capability with **real v0 enforcement** (R-B). A daemon `EVIOCGRAB`s the real evdev
//! device — taking exclusive control so the app can no longer read the raw node — applies the
//! descriptor action-map remap + a rate-limit policy, and re-emits the stream via a `uinput`
//! VIRTUAL device the app reads. The app receives that device's read fd via `Acquire("input")`
//! over a Unix socket (`SCM_RIGHTS`); the fd, not per-event RPC, is the hot path SPIKE-1 (`.1`)
//! settled. This works on the vendor 4.9 A133 kernel TODAY (`UINPUT`/`EVDEV`/`FF_MEMLESS` are
//! `=y`); it needs **no namespaces**, so it is the one piece of E2 enforcement that does not wait
//! on the Phase-2 substrate.
//!
//! ## Why this is genuinely enforcing (unlike the rest of v0)
//!
//! Every other v0 capability is a cooperative in-process facade (R-A: contract, not enforcement).
//! INPUT is different: once the broker holds `EVIOCGRAB` on the source, the kernel delivers that
//! device's events ONLY to the broker. A hostile app that opens the raw node reads nothing — the
//! grab is the kernel-enforced boundary. The broker re-emits a canonicalized stream, so the app
//! also gets a *better* device (positional codes, driver-quirk-free), not just a confined one.
//!
//! ## R-C: the Steam Link blessed-binary exemption
//!
//! Steam Link is BOTH an input consumer AND a `uinput` producer (it creates its own virtual
//! controller for the host). `EVIOCGRAB` on its input would break it (EBUSY / double-input /
//! enumeration loop). So Steam Link is a BLESSED BINARY: the broker opens the source and hands the
//! fd over *without grabbing* (coarse FD-passing), and Steam Link keeps its own `/dev/uinput`.
//! This module's grab path is for genuine broker consumers (e.g. `pf-hwprobe`); the no-grab
//! exemption is documented in `README.md` and surfaced by [`AcquireMode`].
//!
//! ## Honesty / hardware gate
//!
//! The grab + re-emit + fd-handoff + enforcement (app cannot reach the raw node) are proven
//! off-hardware under the E5 sim (`itest/run.sh`, native + `qemu-tsp`). The authoritative
//! on-silicon shared-fd latency (~0.15 ms/event target on the A133) is a HARDWARE GATE (owner
//! return + explicit OK).

pub mod broker;
pub mod evdev;
pub mod ioc;
pub mod policy;
pub mod remap;
pub mod scm;
pub mod uinput;

pub use broker::{acquire_input_fd, read_events_raw, serve_acquire, InputBroker};
pub use policy::TokenBucket;
pub use remap::{Remap, RemapError};
pub use uinput::{AbsInfo, Uinput, UinputSpec};

/// How the broker vends the input device to a consumer (R-C).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireMode {
    /// Genuine broker consumer: the broker `EVIOCGRAB`s the source and hands a re-emit fd
    /// (the default, enforcing path).
    Grabbed,
    /// Blessed binary (Steam Link): the broker hands the source fd WITHOUT grabbing, because the
    /// app is itself a `uinput` producer a grab would break (coarse FD-passing).
    BlessedNoGrab,
}

impl AcquireMode {
    /// The blessed-binary exemption is keyed on the consumer's identity (E3's blessed-binary tier).
    /// v0 recognizes Steam Link by its binary name; the broker daemon passes the resolved mode.
    pub fn for_consumer(name: &str) -> AcquireMode {
        if name.eq_ignore_ascii_case("steamlink") || name.eq_ignore_ascii_case("steam-link") {
            AcquireMode::BlessedNoGrab
        } else {
            AcquireMode::Grabbed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steam_link_is_the_blessed_no_grab_exemption() {
        assert_eq!(AcquireMode::for_consumer("steamlink"), AcquireMode::BlessedNoGrab);
        assert_eq!(AcquireMode::for_consumer("steam-link"), AcquireMode::BlessedNoGrab);
        assert_eq!(AcquireMode::for_consumer("pf-hwprobe"), AcquireMode::Grabbed);
    }
}
