//! The enforcing broker contract: launch-time `app.toml` validation, runtime manifest-ceiling +
//! default-deny + entropy-ungated + quota enforcement, and the OUT-OF-PROCESS backend swap over
//! the `.2` wire (the same client code an app/E6 uses hits the enforcing daemon). Device-free.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use pocketforge::backends::InProcessBackend;
use pocketforge::{Backend, CapError, Descriptor, PermissionState, Pf, QuotaLedger};

use pf_broker::{peer_cred, serve_enforcing_until, AppManifest, EnforcingBackend};

fn descriptor(id: &str) -> Descriptor {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../pocketforge/tests/fixtures")
        .join(format!("{id}-capabilities.toml"));
    Descriptor::load(p).expect("fixture")
}

fn manifest(uses: &[&str]) -> AppManifest {
    let toml = format!(
        "[app]\nid = \"com.test.app\"\nuse = [{}]\n",
        uses.iter().map(|u| format!("\"{u}\"")).collect::<Vec<_>>().join(", ")
    );
    AppManifest::from_toml(&toml).expect("parse app.toml")
}

fn enforcing(device: &str, uses: &[&str]) -> (Arc<EnforcingBackend>, Arc<InProcessBackend>) {
    let desc = Arc::new(descriptor(device));
    let inner = InProcessBackend::shared(desc.clone());
    let validated = manifest(uses).validate(&desc).expect("manifest validates");
    let eb = Arc::new(EnforcingBackend::new(inner.clone(), &validated));
    (eb, inner)
}

// --- launch-time validation -----------------------------------------------------------------

#[test]
fn launch_validator_rejects_over_broad_and_accepts_well_formed() {
    // a523 backs imu+rumble: a sane manifest validates.
    assert!(manifest(&["input", "vibration", "imu", "entropy"]).validate(&descriptor("a523")).is_ok());
    // a133 has no IMU: a REQUIRED imu is an over-broad route → rejected.
    assert!(manifest(&["imu"]).validate(&descriptor("a133")).is_err());
    // …but OPTIONAL imu? is allowed (graceful absence at runtime).
    assert!(manifest(&["imu?"]).validate(&descriptor("a133")).is_ok());
    // an unknown capability is rejected.
    assert!(manifest(&["telepathy"]).validate(&descriptor("a523")).is_err());
}

// --- runtime enforcement: the manifest is the ceiling ---------------------------------------

#[test]
fn undeclared_capability_is_policy_blocked() {
    // Declares only input; vibration/imu are OUTSIDE the ceiling.
    let (eb, _inner) = enforcing("a523", &["input"]);
    assert!(eb.acquire("input").is_ok(), "declared cap acquires");
    assert_eq!(eb.acquire("imu").err(), Some(CapError::PolicyBlocked), "undeclared imu is policy-blocked");
    assert_eq!(eb.query("imu"), PermissionState::Denied, "undeclared cap reads Denied (no leak)");
    // Undeclared haptics is suppressed (cosmetic no-op, never an error).
    assert_eq!(eb.rumble_pulse(40), pocketforge::RumbleStatus::NoopSuppressed);
}

#[test]
fn declared_present_capability_passes_through_to_inner() {
    let (eb, _inner) = enforcing("a523", &["input", "vibration", "imu"]);
    assert!(eb.acquire("imu").is_ok(), "declared + present imu acquires");
    assert_eq!(eb.rumble_pulse(40), pocketforge::RumbleStatus::Fired, "declared rumble fires (a523 has a motor)");
}

#[test]
fn declared_but_hardware_absent_still_degrades_not_crashes() {
    // a133: imu? declared (optional), no hardware → HardwareAbsent (NOT PolicyBlocked, NOT a crash).
    let (eb, _inner) = enforcing("a133", &["imu?"]);
    assert_eq!(eb.acquire("imu").err(), Some(CapError::HardwareAbsent));
}

#[test]
fn entropy_is_the_ungated_exception() {
    // Entropy auto-grants even when NOT declared (non-exhaustible CSPRNG).
    let (eb, _inner) = enforcing("a133", &["input"]);
    assert!(eb.acquire("entropy").is_ok(), "entropy ungated even undeclared");
    assert_eq!(eb.query("entropy"), PermissionState::Granted);
}

#[test]
fn default_deny_is_preserved_under_the_ceiling() {
    // location declared (optional) but it is a default-deny privacy cap with no consent on the
    // synthetic GNSS rig → ConsentDenied (the inner default-deny survives the ceiling).
    let desc = Arc::new(
        Descriptor::from_toml(
            "[identity]\nid=\"g\"\nmanufacturer=\"x\"\nmodel=\"y\"\nsdl_guid=\"00000000000000000000000000000000\"\n[[inputs]]\nid=\"south\"\nkind=\"button\"\nev_type=\"EV_KEY\"\ncode=\"BTN_A\"\n[[sensors]]\nid=\"gnss\"\nkind=\"gnss\"\niio_device=\"gnss0\"\n",
        )
        .unwrap(),
    );
    let inner = InProcessBackend::shared(desc.clone());
    let validated = manifest(&["location:approximate"]).validate(&desc).unwrap();
    let eb = EnforcingBackend::new(inner.clone(), &validated);
    assert_eq!(eb.acquire("location").err(), Some(CapError::ConsentDenied));
    // Granting consent (E3 overlay) then acquiring consumes the location quota.
    inner.set_consent("location", PermissionState::Granted);
    assert!(eb.acquire("location").is_ok(), "consent granted ⇒ acquire ok");
}

#[test]
fn dangerous_capability_quota_is_enforced() {
    let desc = Arc::new(
        Descriptor::from_toml(
            "[identity]\nid=\"g\"\nmanufacturer=\"x\"\nmodel=\"y\"\nsdl_guid=\"00000000000000000000000000000000\"\n[[inputs]]\nid=\"south\"\nkind=\"button\"\nev_type=\"EV_KEY\"\ncode=\"BTN_A\"\n[[sensors]]\nid=\"gnss\"\nkind=\"gnss\"\niio_device=\"gnss0\"\n",
        )
        .unwrap(),
    );
    let inner = InProcessBackend::shared(desc.clone());
    inner.set_consent("location", PermissionState::Granted);
    let validated = manifest(&["location:approximate"]).validate(&desc).unwrap();
    let quotas = Arc::new(QuotaLedger::new());
    quotas.set_remaining("location", 2);
    let eb = EnforcingBackend::with_quotas(inner.clone(), &validated, quotas);
    assert!(eb.acquire("location").is_ok());
    assert!(eb.acquire("location").is_ok());
    assert_eq!(eb.acquire("location").err(), Some(CapError::PolicyBlocked), "quota exhausted");
}

// --- the OUT-OF-PROCESS backend swap over the .2 wire (with enforcement) ---------------------

#[test]
fn out_of_process_client_hits_the_enforced_semantics() {
    let dir = std::env::temp_dir().join(format!("pf-broker-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let sock = dir.join("broker.sock");
    let _ = std::fs::remove_file(&sock);

    let desc = Arc::new(descriptor("a523"));
    let inner = InProcessBackend::shared(desc.clone());
    // App declares input + vibration but NOT imu.
    let validated = manifest(&["input", "vibration"]).validate(&desc).unwrap();
    let eb: Arc<dyn Backend> = Arc::new(EnforcingBackend::new(inner, &validated));

    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let server = std::thread::spawn(move || {
        // peer-uid = our own uid (same-process client passes the SO_PEERCRED check).
        let uid = unsafe { libc::getuid() };
        let _ = serve_enforcing_until(listener, eb, Some(uid), &stop2);
    });

    // The SAME client an app uses (Pf::via_broker) — no app-source change vs the in-process path.
    let pf = Pf::via_broker(desc.clone(), &sock).expect("connect broker");
    assert!(pf.acquire::<pocketforge::Input>().is_ok(), "declared input acquires over the wire");
    // imu is outside the ceiling → PolicyBlocked, observed over the socket.
    assert_eq!(pf.acquire::<pocketforge::Imu>().err(), Some(CapError::PolicyBlocked));
    // entropy ungated even over the wire + undeclared.
    assert!(pf.acquire::<pocketforge::Entropy>().is_ok());

    stop.store(true, Ordering::Release);
    // Nudge the accept loop so it observes `stop` promptly.
    let _ = std::os::unix::net::UnixStream::connect(&sock);
    let _ = server.join();
    let _ = std::fs::remove_file(&sock);
}

// --- SO_PEERCRED -----------------------------------------------------------------------------

#[test]
fn peer_cred_reports_our_own_uid() {
    let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
    let cred = peer_cred(&a).expect("SO_PEERCRED on a socketpair");
    assert_eq!(cred.uid, unsafe { libc::getuid() }, "peer of a socketpair is us");
}
