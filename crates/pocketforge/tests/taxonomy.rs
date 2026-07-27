//! The capability/permission CONTRACT against both real descriptors — the Rust port of the E5
//! sim's `broker_stub.py` assertions. Descriptor-honest graceful missing-hardware degradation,
//! the four-way taxonomy, the cosmetic no-op tier, and the side-effect-free query() shape.

mod common;

use std::sync::Arc;

use pocketforge::backends::InProcessBackend;
use pocketforge::{
    Accelerometer, Backend, CapError, Entropy, Imu, Input, Leds, Location, PermissionState, Pf,
    RumbleStatus, Vibration,
};

// --- a133 (base Pro): no IMU, no rumble, no GNSS ---------------------------------------------

#[test]
fn a133_imu_is_hardware_absent_not_a_crash() {
    let pf = Pf::in_process(common::descriptor("a133"));
    let has = pf.has_capability::<Imu>();
    assert!(has.api, "the Imu capability type exists in the build");
    assert!(!has.hardware, "a133 advertises no IMU");
    assert!(!has.present());
    assert_eq!(pf.acquire::<Imu>().err(), Some(CapError::HardwareAbsent));
    assert_eq!(pf.acquire::<Accelerometer>().err(), Some(CapError::HardwareAbsent));
    // query() is side-effect-free and reports denied (can't grant absent hardware).
    assert_eq!(pf.query::<Imu>(), PermissionState::Denied);
}

#[test]
fn a133_vibration_is_cosmetic_noop_absent() {
    let pf = Pf::in_process(common::descriptor("a133"));
    // Cosmetic tier: acquire NEVER errors, even with no motor.
    let vib = pf.acquire::<Vibration>().expect("vibration acquire is infallible (cosmetic)");
    assert!(!vib.has_motor, "a133 has no rumble motor");
    assert_eq!(vib.pulse(40), RumbleStatus::NoopAbsent);
}

#[test]
fn a133_location_absent() {
    let pf = Pf::in_process(common::descriptor("a133"));
    // a133 has no GNSS at all → hardware-absent (not merely consent-denied).
    assert_eq!(pf.acquire::<Location>().err(), Some(CapError::HardwareAbsent));
    assert_eq!(pf.query::<Location>(), PermissionState::Denied);
}

// --- a523 (Pro S): rumble; IMU + GNSS silicon present but DT-unbound → row-omitted -----------

#[test]
fn a523_imu_is_hardware_absent_qmi8658_dt_present_but_unbound() {
    // Was `a523_imu_present_and_readable`. platform REMOVED the a523 `[[sensors]]` IMU row after
    // SPIKE-0 adjudicated the qmi8658 DT-present but driver-UNBOUND on the operative stock kernel
    // (2026-07-11, two independent on-silicon checks) — R3: missing hardware is a row omission,
    // never a fabricated row. This test kept asserting the opposite for months, green, because it
    // read a vendored copy of the descriptor instead of the descriptor (tsp-ozbp.16).
    let pf = Pf::in_process(common::descriptor("a523"));
    let has = pf.has_capability::<Imu>();
    assert!(has.api, "the Imu capability type still exists in the build");
    assert!(!has.hardware, "a523 advertises no BOUND IMU (qmi8658 driver unbound)");
    assert_eq!(pf.acquire::<Imu>().err(), Some(CapError::HardwareAbsent));
    assert_eq!(pf.query::<Imu>(), PermissionState::Denied);
}

#[test]
fn imu_present_and_readable_on_a_bound_imu() {
    // The acquire/read/query path for a device that DOES have a bound IMU. No shipping device
    // does today, so this runs on the honestly-synthetic rig rather than in a real device's name.
    let pf = Pf::in_process(common::imu_descriptor());
    assert!(pf.has_capability::<Imu>().present(), "the rig has a bound qmi8658");
    let imu = pf.acquire::<Imu>().expect("bound IMU acquires");
    assert!(imu.read_pose().is_ok());
    assert_eq!(pf.query::<Imu>(), PermissionState::Granted);
}

#[test]
fn a523_rumble_fires_then_suppressed_by_haptics_pref() {
    // Drive the shared backend so we can toggle the accessibility preference (the E4 seam).
    let backend = InProcessBackend::shared(Arc::new(common::descriptor("a523")));
    let pf = Pf::over_in_process(backend.clone());

    let vib = pf.acquire::<Vibration>().unwrap();
    assert!(vib.has_motor, "a523 has a pwm-vibrator");
    assert_eq!(vib.pulse(40), RumbleStatus::Fired);

    // Disabling haptics (E4) makes the SAME call a no-op via the SAME path as absence.
    backend.set_preference_bool("hapticsEnabled", false);
    assert_eq!(vib.pulse(40), RumbleStatus::NoopSuppressed);
}

#[test]
fn a523_location_absent_gnss_unbound() {
    // a523 has GNSS silicon but it is DT-but-unbound, so the E1 descriptor OMITS it (descriptor
    // = only-what's-proven). Honest result: location is hardware-absent, not merely consent-gated.
    let pf = Pf::in_process(common::descriptor("a523"));
    assert!(!pf.has_capability::<Location>().present(), "a523 omits GNSS until proven bound");
    assert_eq!(pf.acquire::<Location>().err(), Some(CapError::HardwareAbsent));
    assert_eq!(pf.query::<Location>(), PermissionState::Denied);
}

#[test]
fn synthetic_gnss_is_default_deny_consent() {
    // The privacy default-deny tier is real policy code; exercise it with a GNSS-bearing
    // (synthetic) descriptor since no shipping device advertises GNSS yet.
    let pf = Pf::in_process(common::gnss_descriptor());
    assert!(pf.has_capability::<Location>().present(), "synthetic descriptor advertises GNSS");
    assert_eq!(pf.query::<Location>(), PermissionState::Prompt);
    assert_eq!(pf.acquire::<Location>().err(), Some(CapError::ConsentDenied));
    assert!(!pf.is_granted::<Location>(), "default-deny ⇒ not granted (assert_capability_denied)");
}

// --- platform-constant caps -----------------------------------------------------------------

#[test]
fn entropy_is_ungated_on_both() {
    for id in ["a133", "a523"] {
        let pf = Pf::in_process(common::descriptor(id));
        assert_eq!(pf.query::<Entropy>(), PermissionState::Granted, "{id}: entropy ungated");
        let h = pf.acquire::<Entropy>().expect("entropy acquires");
        let mut buf = [0u8; 32];
        h.fill(&mut buf).expect("entropy fill");
        assert!(buf.iter().any(|&b| b != 0), "{id}: entropy produced bytes");
    }
}

// --- the zero-per-device claim: same code path, descriptor data is the only difference -------

#[test]
fn input_action_map_is_pure_descriptor_data() {
    let a133 = Pf::in_process(common::descriptor("a133"));
    let a523 = Pf::in_process(common::descriptor("a523"));

    let m133 = a133.acquire::<Input>().unwrap();
    let m523 = a523.acquire::<Input>().unwrap();

    // accept_default = "south" on both → confirm resolves to the south face button.
    assert_eq!(m133.map().resolve("confirm"), Some("south"));
    assert_eq!(m133.map().resolve("cancel"), Some("east"));

    // The a523-only controls (home / clickable sticks) appear by DATA, with no per-device code.
    assert!(m133.map().by_id("home").is_none(), "base Pro has no home button");
    assert!(m523.map().by_id("home").is_some(), "Pro S adds a home button (descriptor row)");
    assert!(m523.map().by_id("l3").is_some(), "Pro S adds clickable left stick");

    // Both share the universal face buttons.
    for id in ["south", "east", "west", "north", "dpad", "lstick", "ltrig"] {
        assert!(m133.map().by_id(id).is_some(), "a133 missing {id}");
        assert!(m523.map().by_id(id).is_some(), "a523 missing {id}");
    }
}

#[test]
fn leds_present_on_both_with_descriptor_count() {
    // LEDs are a cosmetic-tier output present on both (different controllers — data, not code).
    let a133 = Pf::in_process(common::descriptor("a133"));
    let a523 = Pf::in_process(common::descriptor("a523"));
    assert_eq!(a133.acquire::<Leds>().unwrap().count, 23, "a133 = 23 sunxi_led");
    assert!(a523.acquire::<Leds>().unwrap().count > 0, "a523 has an led array");
}

// --- one truth: no vendored descriptor copy may come back (tsp-ozbp.16) ----------------------

/// This REPLACES the old `fixtures_track_platform`, which compared a vendored copy against
/// `$PF_PLATFORM_DESCRIPTORS` and **silently returned when that variable was unset** — as it was
/// in every workflow in this repo. It reported `ok` on every run it ever made while comparing
/// nothing, and the copy it was supposed to guard drifted anyway (stale a133 face-button labels,
/// an a523 IMU row platform had deleted). The copy is gone; the suite reads
/// `platform/devices/<id>/capabilities.toml` directly, so there is no longer a second copy TO
/// drift. What remains is keeping one from being reintroduced.
#[test]
fn no_vendored_descriptor_copy_exists() {
    let crates_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/ dir");
    let mut found = Vec::new();
    let mut stack = vec![crates_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if p.is_dir() {
                // `target/` is build output, not a checked-in copy.
                if name != "target" {
                    stack.push(p);
                }
            } else if name.ends_with("capabilities.toml") || name.contains("-capabilities.toml") {
                found.push(p);
            }
        }
    }
    assert!(
        found.is_empty(),
        "a device descriptor copy has reappeared under crates/: {found:?}\n\
         The runtime suite reads platform/devices/<id>/capabilities.toml directly — a vendored \
         snapshot has nothing forcing it to agree with the device truth it mirrors, which is how \
         four tests came to assert an a523 IMU platform had removed (tsp-ozbp.16). If you need a \
         device shape platform does not describe, add an explicitly SYNTHETIC rig in \
         pocketforge::test_support instead."
    );
}

/// The descriptors the suite runs on really are the platform ones — the property every assertion
/// in this file leans on. It fails loudly (never skips) when no platform checkout is resolvable.
#[test]
fn descriptors_come_from_the_platform_checkout() {
    let root = pocketforge::test_support::platform_root()
        .expect("no platform checkout — see crates/pocketforge/tests/README.md (this must not skip)");
    for id in ["a133", "a523"] {
        let path = pocketforge::test_support::try_descriptor_path(id).expect("descriptor path");
        assert!(path.starts_with(&root), "{id} descriptor resolved outside the platform checkout");
        assert_eq!(common::descriptor(id).identity.id, id);
    }
}
