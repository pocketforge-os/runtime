//! Per-capability MANAGER contract (`tsp-e1b.4`) against both real descriptors + the synthetic
//! GNSS rig. Proves: descriptor-honest graceful missing-hardware (never a crash), the sensor
//! physical-model round-trip + mount-matrix pipeline, the E4 haptics no-op unification, ungated
//! entropy, default-deny location, and the location-read ≠ location-send quota accounting. All
//! device-free; real actuator/sensor silicon is the owner-gated hardware leg.

mod common;

use std::sync::Arc;

use pocketforge::backends::InProcessBackend;
use pocketforge::managers::{EGRESS_QUOTA, LOCATION_READ_QUOTA};
use pocketforge::{
    AudioSink, Backend, CapError, HardwareProbe, PermissionState, Pf, Pose, RumbleStatus,
};

const G: f64 = 9.80665;

fn close(a: &[f64; 3], b: &[f64; 3]) {
    for i in 0..3 {
        assert!((a[i] - b[i]).abs() < 1e-9, "axis {i}: {} != {}", a[i], b[i]);
    }
}

// --- sensors: graceful absence + the physical-model round-trip ------------------------------

#[test]
fn a133_sensors_hardware_absent_never_a_crash() {
    let pf = Pf::in_process(common::descriptor("a133"));
    let s = pf.sensors();
    assert!(!s.present(), "a133 advertises no IMU");
    assert_eq!(s.read_pose().err(), Some(CapError::HardwareAbsent));
    assert_eq!(s.read_accel().err(), Some(CapError::HardwareAbsent));
    assert_eq!(s.read_gyro().err(), Some(CapError::HardwareAbsent));
    assert_eq!(s.read_chip_accel().err(), Some(CapError::HardwareAbsent));
}

#[test]
fn a523_sensors_hardware_absent_qmi8658_is_dt_present_but_unbound() {
    // The a523's qmi8658 is DT-present but driver-UNBOUND on the operative stock kernel, so
    // platform OMITS the [[sensors]] row (SPIKE-0, 2026-07-11 — two independent on-silicon
    // checks). Until an owned A523 kernel binds the IIO driver, the Pro S degrades exactly like
    // the base unit. This assertion used to read `assert!(s.present(), "a523 has a qmi8658 IMU")`
    // and stayed green only because the vendored copy still carried the removed row (tsp-ozbp.16).
    let pf = Pf::in_process(common::descriptor("a523"));
    let s = pf.sensors();
    assert!(!s.present(), "a523 advertises no bound IMU (qmi8658 DT-present, driver unbound)");
    assert_eq!(s.read_pose().err(), Some(CapError::HardwareAbsent));
    assert_eq!(s.read_accel().err(), Some(CapError::HardwareAbsent));
    // Rumble is a SEPARATE actuator row the Pro S really does carry — absence is per-capability,
    // not a whole-device downgrade.
    assert!(pf.vibration().has_motor(), "a523 keeps its pwm-vibrator");
}

#[test]
fn imu_pose_roundtrips_through_the_physical_model() {
    // Runs on the SYNTHETIC imu rig: no shipping device advertises a bound IMU today (see
    // `pocketforge::test_support::imu_descriptor`), and asserting this in the a523's name is what
    // the stale copy let us get away with. The physical-model code under test is unchanged.
    let backend = InProcessBackend::shared(Arc::new(common::imu_descriptor()));
    let pf = Pf::over_in_process(backend.clone());
    let s = pf.sensors();
    assert!(s.present(), "the synthetic rig advertises a bound qmi8658");

    // Tilt the top fully away (pitch 90°): gravity reaction rotates onto +Y; spin about Z.
    backend
        .set_pose(Pose { pitch: 90.0, wz: 30.0, ..Pose::default() })
        .expect("the rig accepts a pose");

    close(&s.read_accel().unwrap(), &[0.0, G, 0.0]);
    // gyro is reported in SI rad/s; the pose carried 30 deg/s about Z.
    close(&s.read_gyro().unwrap(), &[0.0, 0.0, 30.0_f64.to_radians()]);

    // The mount-matrix pipeline round-trips (the rig mount is identity): chip → device recovers accel.
    let chip = s.read_chip_accel().unwrap();
    close(&s.device_from_chip(&chip), &s.read_accel().unwrap());
    assert_eq!(s.mount_matrix(), &pocketforge::physical_model::IDENTITY_MOUNT);
}

// --- vibration: the unified no-op shape + the E4 enforcement point ---------------------------

#[test]
fn a133_rumble_is_noop_absent() {
    let pf = Pf::in_process(common::descriptor("a133"));
    let v = pf.vibration();
    assert!(!v.has_motor(), "a133 has no pwm-vibrator");
    assert_eq!(v.pulse(40), RumbleStatus::NoopAbsent);
}

#[test]
fn a523_rumble_fires_then_suppressed_via_settings_e4_path() {
    let backend = InProcessBackend::shared(Arc::new(common::descriptor("a523")));
    let pf = Pf::over_in_process(backend);
    let v = pf.vibration();
    assert!(v.has_motor(), "a523 has a pwm-vibrator");
    assert_eq!(v.pulse(40), RumbleStatus::Fired);

    // The E4 accessibility toggle, applied through the SettingsManager, suppresses the SAME call
    // via the SAME no-op shape as an absent motor (the enforcement point at the primitive).
    pf.settings().set_bool("hapticsEnabled", false);
    assert_eq!(v.pulse(40), RumbleStatus::NoopSuppressed);
}

// --- entropy: ungated -----------------------------------------------------------------------

#[test]
fn entropy_is_ungated_on_both_descriptors() {
    for id in ["a133", "a523"] {
        let pf = Pf::in_process(common::descriptor(id));
        let e = pf.entropy();
        let mut buf = [0u8; 32];
        e.fill(&mut buf).expect("entropy fill is ungated");
        assert!(buf.iter().any(|&b| b != 0), "{id}: entropy produced bytes");
    }
}

// --- location: default-deny + the read≠send accounting split ---------------------------------

#[test]
fn location_is_default_deny_on_synthetic_gnss() {
    let pf = Pf::in_process(common::gnss_descriptor());
    let loc = pf.location();
    assert!(loc.present(), "synthetic descriptor advertises GNSS");
    assert_eq!(loc.query(), PermissionState::Prompt);
    assert_eq!(loc.read_fix().err(), Some(CapError::ConsentDenied));
}

#[test]
fn location_read_and_egress_send_account_into_separate_buckets() {
    let backend = InProcessBackend::shared(Arc::new(common::gnss_descriptor()));
    let pf = Pf::over_in_process(backend.clone());
    // Grant location consent (the E3 overlay) so a read can succeed.
    backend.set_consent("location", PermissionState::Granted);

    let loc = pf.location();
    let eg = pf.egress();
    assert_eq!(loc.reads_remaining(), LOCATION_READ_QUOTA);
    assert_eq!(eg.remaining(), EGRESS_QUOTA);

    // An egress SEND consumes the egress bucket and NEVER the location bucket.
    let receipt = eg.send("steampowered.com", 1500).expect("egress accounted");
    assert_eq!(receipt.host, "steampowered.com");
    assert_eq!(eg.remaining(), EGRESS_QUOTA - 1);
    assert_eq!(loc.reads_remaining(), LOCATION_READ_QUOTA, "egress send leaked into location");

    // A location READ consumes the location bucket and NEVER the egress bucket.
    loc.read_fix().expect("granted location read");
    assert_eq!(loc.reads_remaining(), LOCATION_READ_QUOTA - 1);
    assert_eq!(eg.remaining(), EGRESS_QUOTA - 1, "location read leaked into egress");

    // The audit log records the send (the AppOps-style trail).
    assert_eq!(eg.audit_log().len(), 1);
    assert_eq!(eg.audit_log()[0].bytes, 1500);
}

#[test]
fn egress_quota_exhaustion_is_policy_blocked() {
    let pf = Pf::in_process(common::descriptor("a523"));
    pf.quotas().set_remaining("egress", 1);
    let eg = pf.egress();
    assert!(eg.send("host.example", 1).is_ok());
    assert_eq!(eg.send("host.example", 1).err(), Some(CapError::PolicyBlocked));
}

// --- input: zero-per-device action map ------------------------------------------------------

#[test]
fn input_manager_is_pure_descriptor_data() {
    let a133 = Pf::in_process(common::descriptor("a133"));
    let a523 = Pf::in_process(common::descriptor("a523"));
    let m133 = a133.input_manager();
    let m523 = a523.input_manager();

    assert_eq!(m133.resolve("confirm"), Some("south"));
    assert_eq!(m133.resolve("cancel"), Some("east"));
    // a523-only controls appear by DATA, no per-device code.
    assert!(m133.by_id("home").is_none(), "base Pro has no home button");
    assert!(m523.by_id("home").is_some(), "Pro S adds a home button (descriptor row)");
    assert!(m523.by_id("l3").is_some(), "Pro S adds clickable left stick");
    for id in ["south", "east", "west", "north", "dpad", "lstick", "ltrig"] {
        assert!(m133.by_id(id).is_some(), "a133 missing {id}");
        assert!(m523.by_id(id).is_some(), "a523 missing {id}");
    }
}

// --- audio routing (cooperative platform service) -------------------------------------------

#[test]
fn audio_routes_cooperatively() {
    let pf = Pf::in_process(common::descriptor("a523"));
    let a = pf.audio();
    assert_eq!(a.current(), AudioSink::Speaker, "default route is the speaker");
    a.route(AudioSink::Headphone).expect("route to headphone");
    assert_eq!(a.current(), AudioSink::Headphone);
}

// --- the live-probe reconciliation seam (descriptor=expectation, probe=ground truth) --------

struct ImuUnboundProbe;
impl HardwareProbe for ImuUnboundProbe {
    fn probe_present(&self, name: &str) -> Option<bool> {
        // Model the "DT-but-unbound" hazard: the descriptor advertises the IMU, the live probe
        // finds it absent. Everything else is inconclusive (trust the descriptor).
        if name.eq_ignore_ascii_case("imu") {
            Some(false)
        } else {
            None
        }
    }
}

#[test]
fn live_probe_demotes_an_unbound_imu_to_hardware_absent() {
    // MUST run on a descriptor that actually ADVERTISES an IMU, or there is nothing to demote and
    // the assertion passes for the wrong reason. It used to name the a523 — which platform no
    // longer describes as having a bound IMU, so against the real descriptor this test would have
    // gone vacuously green rather than red: a silent second instance of the very failure class
    // tsp-ozbp.16 is about. The synthetic rig keeps the demotion observable.
    let rig = common::imu_descriptor();
    assert!(rig.cap_present("imu"), "precondition: the rig advertises an IMU to demote");
    let pf = Pf::in_process(rig).with_probe(Arc::new(ImuUnboundProbe));
    let s = pf.sensors();
    assert!(!s.present(), "probe demotes the descriptor-advertised IMU");
    assert_eq!(s.read_pose().err(), Some(CapError::HardwareAbsent));
    // Vibration is unaffected (the probe is inconclusive for rumble ⇒ trust the descriptor).
    assert!(pf.vibration().has_motor());
}
