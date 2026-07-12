//! Shared helpers for `tsp-xubv.4`'s sim end-to-end tests:
//!
//! * [`descriptor`] loads a fixture from the merged `pocketforge` test fixtures — the ONE truth
//!   for hardware presence, so the a133 / a523 rows differ by DATA, not by test code.
//! * [`PayloadApp`] is the running-app abstraction the tests drive: it holds the
//!   `PrefsDidChange` subscription that models "a running app reacted to a live preference
//!   flip", and offers a tiny surface (`observed`, `try_observe`, `pulse`) — the shape a v0 app
//!   sees through the [`pocketforge`] facade. It borrows the facade rather than owning it so the
//!   test bodies keep driving `pf.vibration()` / `pf.audio()` directly (the merged `Pf` is not
//!   `Clone`; borrowing dodges that without touching the merged crate).
//! * [`run_pf_settings`] spawns the built `pf-settings` binary as a **real subprocess** pointed at
//!   an isolated `$PF_PREFS_DIR` — the ratified `.2` HONESTY RIDER's external-writer leg. This is
//!   the load-bearing distinction from the `.2` unit tests, which used a second in-process
//!   `PrefsStore` handle.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use pf_prefs::PrefValue;
use pocketforge::{Descriptor, Pf, RumbleStatus};

/// Load a fixture descriptor from the `pocketforge` crate's shared test fixtures. Reusing the
/// merged fixtures (rather than defining a parallel set here) keeps the a133 / a523 truth in ONE
/// place — descriptor drift would immediately surface as a broken test on both sides.
pub fn descriptor(id: &str) -> Descriptor {
    // pf-settings sits at `crates/pf-settings`; the pocketforge fixtures are at
    // `crates/pocketforge/tests/fixtures/`. CARGO_MANIFEST_DIR is the pf-settings package root.
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("pocketforge")
        .join("tests")
        .join("fixtures")
        .join(format!("{id}-capabilities.toml"));
    Descriptor::load(&fixture).unwrap_or_else(|e| {
        panic!("load fixture descriptor {}: {e}", fixture.display())
    })
}

/// A scratch `$PF_PREFS_DIR` unique to this process + tag, wiped fresh so no prior run's state
/// bleeds in. Callers pair this with [`cleanup`] on the happy path — a failed test intentionally
/// leaves the directory for post-mortem inspection.
pub fn scratch_prefs_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join(format!("pf-xubv4-e2e-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Best-effort teardown; ignores errors so a race with an unresponsive filesystem cannot mask a
/// test success.
pub fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// Spawn the built `pf-settings` binary against a scratch prefs dir. The `PF_PREFS_DIR` override
/// is the ONLY resolver source for the test — every other env-derived path is neutralized so a
/// developer's `XDG_STATE_HOME` cannot leak into the run.
pub fn run_pf_settings(prefs_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pf-settings"))
        .args(args)
        .env("PF_PREFS_DIR", prefs_dir)
        .env_remove("XDG_STATE_HOME")
        .env_remove("HOME")
        .output()
        .expect("spawn pf-settings")
}

/// A minimal "running app" abstraction: it holds a `PrefsDidChange` subscription taken from the
/// facade's [`Pf::settings`] surface, mirroring what a real v0 app would open. It borrows the
/// [`Pf`] because the merged `Pf` is not `Clone` — callers keep driving `pf.vibration()` /
/// `pf.audio()` directly in the test body while `PayloadApp` owns the observer + surfaces the
/// wait-for-change API.
pub struct PayloadApp<'pf> {
    pf: &'pf Pf,
    observer: Receiver<PrefValue>,
}

impl<'pf> PayloadApp<'pf> {
    /// Start the payload app against `pf` and a preference key: opens the `PrefsDidChange`
    /// subscription. Returns `None` on a backend that cannot observe in-process (the v0 broker
    /// client, per `Backend::subscribe_preference`'s contract); the in-process backend these
    /// tests wire is the one that CAN observe, so callers unwrap with an "observer available"
    /// message.
    pub fn start(pf: &'pf Pf, pref: &str) -> Option<PayloadApp<'pf>> {
        let observer = pf.settings().subscribe(pref)?;
        Some(PayloadApp { pf, observer })
    }

    /// One rumble actuation attempt (40 ms — the shape used across the merged tests).
    /// This IS the app-visible call: the primitive resolves to `Fired` / `NoopAbsent` /
    /// `NoopSuppressed` at the point of actuation, honoring both hardware presence and the
    /// current `hapticsEnabled` preference. NO app-side branching on either.
    pub fn pulse(&self) -> RumbleStatus {
        self.pf.vibration().pulse(40)
    }

    /// Wait for a preference change event, failing with `RecvTimeoutError` if none arrives inside
    /// `budget`. This is the "the running app reacted live" assertion surface.
    pub fn observed(&self, budget: Duration) -> Result<PrefValue, RecvTimeoutError> {
        self.observer.recv_timeout(budget)
    }

    /// A pre-reload timeout probe — the running app MUST NOT observe an event before the host
    /// picks the external write up (v0 semantics). Callers `assert_eq!` against
    /// `Err(RecvTimeoutError::Timeout)` to prove the pre-reload silence.
    pub fn try_observe(&self, budget: Duration) -> Result<PrefValue, RecvTimeoutError> {
        self.observer.recv_timeout(budget)
    }
}
