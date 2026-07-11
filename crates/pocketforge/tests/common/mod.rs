//! Shared test helpers: fixture loading + a deterministic capability "snapshot" used to prove
//! that the in-process and out-of-process backends behave IDENTICALLY (the backend-swap proof).

#![allow(dead_code)]

use std::path::PathBuf;

use pocketforge::{
    CapError, Descriptor, PermissionState, Pf, RumbleStatus,
};

/// The vendored fixtures directory.
pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures")
}

/// Path to a fixture descriptor (`"a133"` / `"a523"`).
pub fn fixture_path(id: &str) -> PathBuf {
    fixtures_dir().join(format!("{id}-capabilities.toml"))
}

/// Load a fixture descriptor.
pub fn descriptor(id: &str) -> Descriptor {
    Descriptor::load(fixture_path(id)).expect("load fixture descriptor")
}

/// A SYNTHETIC descriptor that advertises GNSS, used to exercise the default-deny / consent
/// POLICY (real code) — neither shipping device (a133/a523) advertises GNSS today (DT-unbound
/// on both SoCs per SPIKE-0 `tsp-9sx.1`, so the E1 descriptors omit it: descriptor = only-
/// what's-proven). This stands in for a future GNSS-bearing device so the privacy-tier state
/// machine is still tested.
///
/// `[[sensors]] kind = "gnss"` is now schema-representable (E1 `capabilities.schema.json` post-
/// `tsp-9sx.6`) and the row honestly OMITS `iio_device` (GNSS is not an IIO sink — gpsd/NMEA/
/// CUSE stream, not iio sysfs).
pub fn gnss_descriptor() -> Descriptor {
    Descriptor::from_toml(
        r#"
[identity]
id = "synthgnss"
manufacturer = "PocketForge"
model = "GNSS Policy Rig (synthetic test descriptor)"
sdl_guid = "00000000000000000000000000000000"

[[inputs]]
id = "south"
kind = "button"
ev_type = "EV_KEY"
code = "BTN_A"

[[sensors]]
id = "imu"
kind = "accel+gyro"
iio_device = "qmi8658"

[[sensors]]
id = "gnss"
kind = "gnss"
"#,
    )
    .expect("parse synthetic gnss descriptor")
}

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
