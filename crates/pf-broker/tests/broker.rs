//! The enforcing broker contract: launch-time `app.toml` validation, runtime manifest-ceiling +
//! default-deny + entropy-ungated + quota enforcement, and the OUT-OF-PROCESS backend swap over
//! the `.2` wire (the same client code an app/E6 uses hits the enforcing daemon). Device-free.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use pocketforge::backends::InProcessBackend;
use pocketforge::{Backend, CapError, Descriptor, PermissionState, Pf, QuotaLedger};

use pf_broker::{
    peer_cred, serve_enforcing_until, AppManifest, BlessedRegistration, EnforcingBackend, LaunchTrust,
    Violation,
};

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

// --- E3 protection tiers: signature-tier launch gate + blessed-binary exemption (tsp-ht0p.2) --

fn manifest_for(app_id: &str, uses: &[&str]) -> AppManifest {
    let toml = format!(
        "[app]\nid = \"{app_id}\"\nuse = [{}]\n",
        uses.iter().map(|u| format!("\"{u}\"")).collect::<Vec<_>>().join(", ")
    );
    AppManifest::from_toml(&toml).expect("parse app.toml")
}

/// A synthetic descriptor that backs `location` (gnss) AND `vibration` (a rumble motor) — the
/// shipped a133/a523 fixtures omit GNSS, so the bead's STEP-1 well-formed list needs this. (No
/// `iio_device` — GNSS is DT-unbound on both SoCs and `iio_device` is optional as of runtime#13;
/// `location` presence derives from the sensor `kind`, not the node.)
fn desc_gnss_and_rumble() -> Descriptor {
    Descriptor::from_toml(
        "[identity]\nid=\"step1\"\nmanufacturer=\"x\"\nmodel=\"y\"\nsdl_guid=\"00000000000000000000000000000000\"\n\
         [[inputs]]\nid=\"south\"\nkind=\"button\"\nev_type=\"EV_KEY\"\ncode=\"BTN_A\"\n\
         [[sensors]]\nid=\"gnss\"\nkind=\"gnss\"\n\
         [[actuators]]\nid=\"rumble\"\nkind=\"rumble\"\nev_type=\"EV_FF\"\ncode=\"FF_RUMBLE\"\nsysfs=\"pwm-vibrator\"\n",
    )
    .expect("synthetic descriptor parses")
}

#[test]
fn step1_well_formed_manifest_passes_launch_validation() {
    // Bead STEP-1 (well-formed): entropy(normal/ungated) + vibration(normal, backed by a motor) +
    // location:approximate(dangerous, backed by gnss) + egress:<specific-host>(dangerous, declared)
    // — ALL pass launch validation for an untrusted app (dangerous ≠ signature; no trust needed).
    let desc = desc_gnss_and_rumble();
    let v = manifest(&["entropy", "vibration", "location:approximate", "egress:tile.example"])
        .validate(&desc)
        .expect("well-formed manifest validates");
    assert!(v.allows("entropy") && v.allows("vibration") && v.allows("location") && v.allows("egress"));
    assert_eq!(v.egress_hosts().collect::<Vec<_>>(), ["tile.example"]);
}

#[test]
fn step1_broad_egress_from_untrusted_app_is_signature_tier_reject() {
    // Bead STEP-1 (reject): raw/arbitrary egress (0.0.0.0/0) from a NON-first-party app is a TYPED
    // signature-tier launch REJECT — not an unknown-cap / over-broad-route reason.
    let err = manifest(&["egress:0.0.0.0/0"]).validate(&descriptor("a133")).unwrap_err();
    assert_eq!(err, vec![Violation::SignatureTierRequiresTrust { token: "egress:0.0.0.0/0".into() }]);
}

#[test]
fn first_party_signed_app_clears_the_signature_tier() {
    // The SAME broad-egress manifest passes when the app carries first-party (release-key) trust.
    let v = manifest(&["egress:0.0.0.0/0"])
        .validate_with_trust(&descriptor("a133"), &LaunchTrust::FIRST_PARTY)
        .expect("first-party app may declare raw egress");
    assert!(v.allows("egress"));
    assert_eq!(v.egress_hosts().collect::<Vec<_>>(), ["0.0.0.0/0"]);
}

#[test]
fn blessed_binary_exemption_clears_signature_tier_only_when_enumerated() {
    // R-C blessed-binary (Steam Link): an enumerated, first-party-SIGNED broad-grant clears the
    // signature tier for the matching app_id ONLY — via the enumeration, and only for that app.
    let reg = BlessedRegistration {
        app_id: "com.valve.steamlink".into(),
        sha256: "deadbeefcafe".into(),
        grants: vec!["egress:0.0.0.0/0".into()],
    };
    let steamlink = manifest_for("com.valve.steamlink", &["egress:0.0.0.0/0"]);

    // Positive: blessed + enumerated + matching app_id ⇒ passes ONLY via the exemption path.
    assert!(steamlink.validate_with_trust(&descriptor("a133"), &LaunchTrust::blessed(&reg)).is_ok());

    // Negative 1 — the SAME grant WITHOUT the blessed registration (untrusted) ⇒ reject.
    assert_eq!(
        steamlink.validate(&descriptor("a133")).unwrap_err(),
        vec![Violation::SignatureTierRequiresTrust { token: "egress:0.0.0.0/0".into() }]
    );
    // Negative 2 — a blessed registration for a DIFFERENT app does not carry over.
    let copycat = manifest_for("com.evil.copycat", &["egress:0.0.0.0/0"]);
    assert!(copycat.validate_with_trust(&descriptor("a133"), &LaunchTrust::blessed(&reg)).is_err());
    // Negative 3 — blessed for the right app but the grant is NOT enumerated ⇒ reject.
    let narrow = BlessedRegistration {
        app_id: "com.valve.steamlink".into(),
        sha256: "deadbeefcafe".into(),
        grants: vec!["egress:tile.example".into()],
    };
    assert!(steamlink.validate_with_trust(&descriptor("a133"), &LaunchTrust::blessed(&narrow)).is_err());
}

#[test]
fn specific_host_egress_is_dangerous_not_signature() {
    // egress:<specific-host> is Dangerous (declared; runtime consent/default-deny lands via .3's
    // generic dangerous-tier flow), NOT signature — an untrusted app may DECLARE it at launch.
    let v = manifest(&["egress:steampowered.com"]).validate(&descriptor("a133")).expect("declared host ok");
    assert!(v.allows("egress"));
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
            "[identity]\nid=\"g\"\nmanufacturer=\"x\"\nmodel=\"y\"\nsdl_guid=\"00000000000000000000000000000000\"\n[[inputs]]\nid=\"south\"\nkind=\"button\"\nev_type=\"EV_KEY\"\ncode=\"BTN_A\"\n[[sensors]]\nid=\"gnss\"\nkind=\"gnss\"\n",
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
            "[identity]\nid=\"g\"\nmanufacturer=\"x\"\nmodel=\"y\"\nsdl_guid=\"00000000000000000000000000000000\"\n[[inputs]]\nid=\"south\"\nkind=\"button\"\nev_type=\"EV_KEY\"\ncode=\"BTN_A\"\n[[sensors]]\nid=\"gnss\"\nkind=\"gnss\"\n",
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
