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

// --- a523 (Pro S): IMU + rumble + GNSS -------------------------------------------------------

#[test]
fn a523_imu_present_and_readable() {
    let pf = Pf::in_process(common::descriptor("a523"));
    assert!(pf.has_capability::<Imu>().present(), "a523 has a qmi8658 IMU");
    let imu = pf.acquire::<Imu>().expect("a523 IMU acquires");
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

// --- drift guard: vendored fixtures vs the live platform descriptors (CI-gated) -------------

#[test]
fn fixtures_track_platform() {
    let Some(root) = std::env::var_os("PF_PLATFORM_DESCRIPTORS") else {
        eprintln!("skip: set PF_PLATFORM_DESCRIPTORS=<platform/devices> to check fixture drift");
        return;
    };
    for id in ["a133", "a523"] {
        let live = std::path::Path::new(&root).join(id).join("capabilities.toml");
        let live = std::fs::read_to_string(&live)
            .unwrap_or_else(|e| panic!("read live {id} descriptor: {e}"));
        let fixture = std::fs::read_to_string(common::fixture_path(id)).unwrap();
        assert_eq!(fixture, live, "fixture {id} drifted from platform — refresh it");
    }
}
