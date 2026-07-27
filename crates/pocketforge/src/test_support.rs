//! Dev/test-only helpers for locating the **real** `platform` device descriptors (`tsp-ozbp.16`).
//!
//! This module exists so the runtime test suite has exactly ONE source of device truth: the
//! `platform` repo's `devices/<id>/capabilities.toml`. Before `tsp-ozbp.16` the suite ran against
//! a hand-copied vendored snapshot under `crates/pocketforge/tests/fixtures/`, and nothing forced
//! the copy to agree with the original — the two-copies-of-one-truth defect the parent bead
//! (`tsp-ozbp`) exists to kill. The copy is gone; the tests read the descriptor the device
//! actually ships against.
//!
//! **The load-bearing property is that resolution FAILS LOUDLY, never silently.** A resolver that
//! returned "no platform checkout, skip" would recreate the exact failure it replaced: a suite
//! that is green because it asserted nothing. [`descriptor`] panics with an actionable message
//! when the platform checkout is missing, and [`try_descriptor`] exposes the same decision as a
//! `Result` so the failure path itself is unit-testable (see the tests at the bottom of this
//! file) rather than being a behaviour nobody has ever observed.
//!
//! Not part of the app-facing facade — `#[doc(hidden)]`, dev-time only. It is compiled
//! unconditionally rather than feature-gated because `pocketforge`'s own integration tests link
//! the crate as an ordinary dependency (so `cfg(test)` is not set for the library), and a
//! `required-features` dance across every `[[test]]` target buys nothing for ~60 lines of path
//! resolution with no dependencies beyond `std`.

use std::path::{Path, PathBuf};

use crate::descriptor::Descriptor;

/// Env var naming the `platform` repo ROOT (not the `devices/` subdirectory).
///
/// Same variable the `pf-input-collect` / `pf-collect-ui` suites and `.github/workflows/
/// runtime-tests.yml` already use, so one checkout + one env var serves every runtime test that
/// needs platform assets.
pub const PLATFORM_DIR_ENV: &str = "PF_PLATFORM_DIR";

/// Relative locations of a sibling `platform` checkout, tried in order, resolved against a
/// starting directory (this crate's manifest dir in [`platform_root`]).
///
/// `../../../platform` is the layout CI and `pf-wt` produce (`<ws>/runtime/crates/pocketforge`
/// beside `<ws>/platform`); the shorter forms cover a repo checked out one or two levels up.
const SIBLING_CANDIDATES: &[&str] = &["../../../platform", "../../platform", "../platform"];

/// Is `root` a plausible `platform` checkout? Keyed on the descriptors this module serves, so a
/// directory that cannot answer the question is never accepted as the answer.
fn looks_like_platform(root: &Path) -> bool {
    root.join("devices").is_dir()
}

/// Resolve the `platform` repo root from an explicit override plus a directory to search upward
/// from. Split out from [`platform_root`] so both the found and NOT-found paths are testable
/// without mutating process env.
pub fn resolve_platform_root(env_dir: Option<&Path>, search_from: &Path) -> Option<PathBuf> {
    if let Some(dir) = env_dir {
        // An explicitly-pointed-at directory that is NOT a platform checkout is a caller error
        // worth surfacing, not something to quietly fall back from.
        if looks_like_platform(dir) {
            return Some(dir.to_path_buf());
        }
        return None;
    }
    for rel in SIBLING_CANDIDATES {
        let candidate = search_from.join(rel);
        if looks_like_platform(&candidate) {
            return Some(candidate.canonicalize().unwrap_or(candidate));
        }
    }
    None
}

/// The `platform` repo root for this test run: `$PF_PLATFORM_DIR` if set, else a sibling checkout.
pub fn platform_root() -> Option<PathBuf> {
    let env_dir = std::env::var_os(PLATFORM_DIR_ENV).map(PathBuf::from);
    resolve_platform_root(env_dir.as_deref(), Path::new(env!("CARGO_MANIFEST_DIR")))
}

/// The message a caller sees when no `platform` checkout is resolvable. A pure function so the
/// "what does the failure actually tell you" property is testable on every run, not only on the
/// runs that happen to be misconfigured.
fn missing_platform_error() -> String {
    format!(
        "no `platform` checkout found, so the a133/a523 device descriptors cannot be read.\n\
         The runtime test suite reads `platform/devices/<id>/capabilities.toml` DIRECTLY — there \
         is deliberately no vendored copy in this repo (tsp-ozbp.16), because a copy drifts \
         silently from the device truth it claims to mirror.\n\
         Fix: clone https://github.com/pocketforge-os/platform beside this repo, or set \
         {PLATFORM_DIR_ENV}=/path/to/platform.\n\
         (Searched {PLATFORM_DIR_ENV}, then {SIBLING_CANDIDATES:?} relative to {manifest}.)",
        manifest = env!("CARGO_MANIFEST_DIR"),
    )
}

/// Path to a device's authoritative capability descriptor, or an actionable error.
pub fn try_descriptor_path(id: &str) -> Result<PathBuf, String> {
    let root = platform_root().ok_or_else(missing_platform_error)?;
    let path = root.join("devices").join(id).join("capabilities.toml");
    if !path.is_file() {
        return Err(format!(
            "platform checkout at {} has no descriptor for device {id:?} (expected {}). \
             Either the device id is wrong or the checkout is stale — refresh it, do NOT vendor a copy.",
            root.display(),
            path.display(),
        ));
    }
    Ok(path)
}

/// Load a device's authoritative descriptor straight from the `platform` checkout.
///
/// Panics — deliberately — when the checkout is absent. A skip here would be a test that cannot
/// fail, which is precisely what this module replaced.
pub fn descriptor(id: &str) -> Descriptor {
    let path = try_descriptor_path(id).unwrap_or_else(|e| panic!("{e}"));
    Descriptor::load(&path)
        .unwrap_or_else(|e| panic!("load platform descriptor {}: {e:?}", path.display()))
}

/// [`descriptor`] as a `Result` — for callers that want to assert on the failure itself.
pub fn try_descriptor(id: &str) -> Result<Descriptor, String> {
    let path = try_descriptor_path(id)?;
    Descriptor::load(&path).map_err(|e| format!("load platform descriptor {}: {e:?}", path.display()))
}

// --- SYNTHETIC descriptors -------------------------------------------------------------------
//
// One home for every made-up device the suite uses, so "is this a real device or a rig?" is
// answerable by where the descriptor came from. A synthetic rig exercises a capability's CODE
// when no shipping device carries that hardware; it must NEVER be used to make a real device
// appear to have hardware it lacks — that is the failure the vendored copy produced.

/// A SYNTHETIC descriptor that advertises GNSS, used to exercise the default-deny / consent
/// POLICY (real code) — neither shipping device (a133/a523) advertises GNSS today (DT-unbound on
/// both SoCs per SPIKE-0 `tsp-9sx.1`, so the E1 descriptors omit it: descriptor = only-what's-
/// proven). This stands in for a future GNSS-bearing device so the privacy-tier state machine is
/// still tested.
///
/// `[[sensors]] kind = "gnss"` is schema-representable (E1 `capabilities.schema.json` post-
/// `tsp-9sx.6`) and the row honestly OMITS `iio_device` (GNSS is not an IIO sink — gpsd/NMEA/CUSE
/// stream, not iio sysfs).
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

/// A SYNTHETIC descriptor carrying a BOUND 6-axis IMU plus a rumble motor, used to exercise the
/// inertial-sensor code path (pose round-trip, mount matrix, live-probe demotion).
///
/// NEITHER shipping device advertises an IMU today. The a133 never had one; the a523's `qmi8658`
/// is DT-present but driver-UNBOUND on the operative stock kernel, adjudicated on silicon by two
/// independent SPIKE-0 checks (2026-07-11), so `platform/devices/a523/capabilities.toml` OMITS the
/// row per the R3 rule (missing hardware = row omission, never a fabricated row).
///
/// Before `tsp-ozbp.16` these tests ran against a vendored a523 snapshot that still carried the
/// removed row — so they asserted, in the device's name, a capability the device does not expose.
/// The honest split is the one [`gnss_descriptor`] already uses: exercise the POLICY/plumbing on a
/// clearly-synthetic device, and let the a523 tests assert the a523's real (absent) state. When an
/// owned A523 kernel binds the driver and platform restores the row, these move back onto the real
/// descriptor.
pub fn imu_descriptor() -> Descriptor {
    Descriptor::from_toml(
        r#"
[identity]
id = "synthimu"
manufacturer = "PocketForge"
model = "Inertial Policy Rig (synthetic test descriptor)"
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
units = "m/s^2,rad/s"
mount_matrix = [[1, 0, 0], [0, 1, 0], [0, 0, 1]]   # identity: the round-trip must be a no-op

[[actuators]]
id = "rumble"
kind = "rumble"
controller = "pwm-vibrator"
"#,
    )
    .expect("parse synthetic imu descriptor")
}

/// A SYNTHETIC descriptor whose triggers are genuinely ANALOG (no `semantics = "binary"`), used to
/// prove the broker's binary reclassification does not touch proportional axes.
///
/// Both shipping devices now describe L2/R2 as `semantics = "binary"` — a binary switch reported
/// on an analog wire channel (SPIKE-0, 2026-07-11: endpoint-only full-swing `ABS_Z`/`ABS_RZ`, no
/// proportional travel). The a523 half of `analog_axis_passes_through_unchanged` used to get its
/// "analog" trigger from the stale vendored copy, i.e. from a device shape that no longer exists.
pub fn analog_trigger_descriptor() -> Descriptor {
    Descriptor::from_toml(
        r#"
[identity]
id = "synthanalog"
manufacturer = "PocketForge"
model = "Analog Trigger Rig (synthetic test descriptor)"
sdl_guid = "030000005e0400008e02000010010000"

[[inputs]]
id = "south"
kind = "button"
ev_type = "EV_KEY"
code = "BTN_A"

[[inputs]]
id = "ltrig"
kind = "trigger"
ev_type = "EV_ABS"
code = "ABS_Z"
range = { min = 0, max = 255, fuzz = 0, flat = 0 }

[[inputs]]
id = "rtrig"
kind = "trigger"
ev_type = "EV_ABS"
code = "ABS_RZ"
range = { min = 0, max = 255, fuzz = 0, flat = 0 }
"#,
    )
    .expect("parse synthetic analog-trigger descriptor")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The NOT-found path is the one that must never degrade into a skip: prove it reports
    /// "absent" rather than inventing a root.
    #[test]
    fn a_directory_that_is_not_a_platform_checkout_resolves_to_nothing() {
        // Deep enough that none of SIBLING_CANDIDATES can climb out into a real checkout — CI runs
        // with `platform` mounted at well-known paths and the search must not accidentally hit it.
        let empty = std::env::temp_dir().join(format!("pf-no-platform-{}/a/b/c/d", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(resolve_platform_root(Some(&empty), &empty), None, "explicit non-platform dir");
        assert_eq!(resolve_platform_root(None, &empty), None, "no sibling platform checkout");
        let _ = std::fs::remove_dir_all(
            std::env::temp_dir().join(format!("pf-no-platform-{}", std::process::id())),
        );
    }

    /// The absent-checkout path must hand back a fix, not a shrug. Asserted on the message itself
    /// so it is checked on every run — including the runs where a checkout IS present, which is
    /// every run in practice and exactly when a rotted error message would go unnoticed.
    #[test]
    fn the_absent_checkout_error_names_the_fix() {
        let msg = missing_platform_error();
        assert!(msg.contains(PLATFORM_DIR_ENV), "names the env var: {msg}");
        assert!(msg.contains("pocketforge-os/platform"), "names what to clone: {msg}");
        assert!(msg.contains("no vendored copy"), "explains why there is nothing to fall back to");
    }

    /// A pointed-at checkout wins over any sibling search.
    #[test]
    fn an_explicit_platform_dir_is_honored() {
        let tmp = std::env::temp_dir().join(format!("pf-test-support-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("devices/a133")).unwrap();
        assert_eq!(
            resolve_platform_root(Some(&tmp), Path::new("/nonexistent")),
            Some(tmp.clone()),
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// In a configured environment (CI, or a sibling checkout) the real descriptors resolve.
    /// Skipping is not an option: this is the property the suite depends on.
    #[test]
    fn the_real_platform_descriptors_resolve() {
        for id in ["a133", "a523"] {
            let d = descriptor(id);
            assert_eq!(d.identity.id, id, "descriptor {id} identifies itself");
        }
    }
}
