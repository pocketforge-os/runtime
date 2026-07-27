//! Shared test helpers: descriptor loading + a deterministic capability "snapshot" used to prove
//! that the in-process and out-of-process backends behave IDENTICALLY (the backend-swap proof).

#![allow(dead_code)]

use pocketforge::{
    CapError, Descriptor, PermissionState, Pf, RumbleStatus,
};

/// Load a REAL device descriptor (`"a133"` / `"a523"`) straight from the `platform` checkout.
///
/// There is no vendored copy in this repo (`tsp-ozbp.16`): a snapshot drifts silently from the
/// device truth it claims to mirror, which is exactly how four tests came to assert an a523 IMU
/// platform had already removed. Resolution PANICS rather than skips when no checkout is found —
/// see `pocketforge::test_support` and `tests/README.md`.
pub fn descriptor(id: &str) -> Descriptor {
    pocketforge::test_support::descriptor(id)
}

/// The SYNTHETIC descriptor rigs, re-exported from their single home in
/// `pocketforge::test_support` so `pf-input-broker`'s unit tests use the same definitions rather
/// than a second copy of them (`tsp-ozbp.16` — the whole point of this bead is one home per
/// truth). See that module for what each rig stands in for and why.
// `common` is compiled into every integration test binary; each uses a different subset.
#[allow(unused_imports)]
pub use pocketforge::test_support::{analog_trigger_descriptor, gnss_descriptor, imu_descriptor};

/// The capability names probed by [`snapshot`], in a fixed order.
pub const PROBE_CAPS: &[&str] = &[
    "input",
    "vibration",
    "imu",
    "accelerometer",
    "gyroscope",
    "magnetometer",
    "location",
    "gnss",
    "entropy",
    "leds",
    "audio",
    "settings",
];

fn perm_str(p: PermissionState) -> &'static str {
    match p {
        PermissionState::Granted => "granted",
        PermissionState::Denied => "denied",
        PermissionState::Prompt => "prompt",
    }
}

fn acq_str(r: Result<(), CapError>) -> &'static str {
    match r {
        Ok(()) => "ok",
        Err(CapError::Unsupported) => "unsupported",
        Err(CapError::PolicyBlocked) => "policy-blocked",
        Err(CapError::ConsentDenied) => "consent-denied",
        Err(CapError::HardwareAbsent) => "hardware-absent",
    }
}

fn rumble_str(r: RumbleStatus) -> &'static str {
    match r {
        RumbleStatus::Fired => "fired",
        RumbleStatus::NoopAbsent => "noop-absent",
        RumbleStatus::NoopSuppressed => "noop-suppressed",
    }
}

/// A deterministic, backend-agnostic readout of the whole capability surface. Two `Pf`s over
/// different backends but the same descriptor MUST produce byte-identical snapshots — that is
/// the operational definition of "the backend is a swap, not a rewrite".
pub fn snapshot(pf: &Pf) -> String {
    let mut out = String::new();
    for &cap in PROBE_CAPS {
        let present = pf.backend().is_present(cap);
        let granted = pf.backend().is_granted(cap);
        let query = perm_str(pf.backend().query(cap));
        let acquire = acq_str(pf.acquire_by_name(cap));
        out.push_str(&format!(
            "{cap}: present={present} granted={granted} query={query} acquire={acquire}\n"
        ));
    }
    out.push_str(&format!("rumble.pulse(40)={}\n", rumble_str(pf.backend().rumble_pulse(40))));
    out.push_str(&format!(
        "imu.get_pose={}\n",
        match pf.backend().get_pose() {
            Ok(_) => "ok",
            Err(e) => match e {
                CapError::HardwareAbsent => "hardware-absent",
                _ => "err",
            },
        }
    ));
    out
}
