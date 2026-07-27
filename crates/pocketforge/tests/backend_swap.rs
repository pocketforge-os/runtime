//! THE LOAD-BEARING PROOF: the v0 in-process backend and the out-of-process broker (PFW1 over
//! a real Unix socket) are a BACKEND SWAP behind ONE facade. The same app code, run against
//! both, produces byte-identical behavior. This is "it survives the runtime fork" in a test.

mod common;

use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use pocketforge::backends::InProcessBackend;
use pocketforge::{server, Backend, Descriptor, Pf, Pose};

static SOCK_SEQ: AtomicU32 = AtomicU32::new(0);

/// Bind a fresh Unix socket and serve the reference broker (wrapping an in-process backend over
/// `descriptor`) on a background thread. Returns (socket path, the server's backend).
///
/// Takes a `Descriptor` rather than a device id so a test can drive the wire with a SYNTHETIC rig
/// when no shipping device carries the hardware it needs (`tsp-ozbp.16`).
fn start_ref_broker_for(
    descriptor: Descriptor,
    label: &str,
) -> (std::path::PathBuf, Arc<InProcessBackend>) {
    let n = SOCK_SEQ.fetch_add(1, Ordering::Relaxed);
    let sock =
        std::env::temp_dir().join(format!("pf-swap-{}-{}-{}.sock", label, std::process::id(), n));
    let _ = std::fs::remove_file(&sock);

    let backend = InProcessBackend::shared(Arc::new(descriptor));
    let server_backend: Arc<dyn Backend> = backend.clone();
    let listener = UnixListener::bind(&sock).expect("bind ref-broker socket");
    std::thread::spawn(move || {
        let _ = server::serve(listener, server_backend);
    });
    (sock, backend)
}

/// [`start_ref_broker_for`] over a REAL device descriptor from the platform checkout.
fn start_ref_broker(id: &str) -> (std::path::PathBuf, Arc<InProcessBackend>) {
    start_ref_broker_for(common::descriptor(id), id)
}

/// Build a `Pf` over the out-of-process broker at `sock` for `descriptor`.
fn broker_pf_for(descriptor: Descriptor, sock: &std::path::Path) -> Pf {
    Pf::via_broker(Arc::new(descriptor), sock).expect("connect broker client")
}

/// [`broker_pf_for`] over a REAL device descriptor from the platform checkout.
fn broker_pf(id: &str, sock: &std::path::Path) -> Pf {
    broker_pf_for(common::descriptor(id), sock)
}

#[test]
fn in_process_and_broker_snapshots_are_identical() {
    for id in ["a133", "a523"] {
        let inproc = Pf::in_process(common::descriptor(id));
        let (sock, _backend) = start_ref_broker(id);
        let broker = broker_pf(id, &sock);

        let a = common::snapshot(&inproc);
        let b = common::snapshot(&broker);
        assert_eq!(
            a, b,
            "{id}: in-process and broker snapshots differ — the backend is NOT a clean swap\n\
             --- in-process ---\n{a}\n--- broker ---\n{b}"
        );
        let _ = std::fs::remove_file(&sock);
    }
}

#[test]
fn broker_reports_a133_missing_hardware_like_in_process() {
    // Spot-check the headline contract specifically over the wire (not just via the snapshot).
    let (sock, _b) = start_ref_broker("a133");
    let pf = broker_pf("a133", &sock);
    assert!(!pf.backend().is_present("imu"), "broker: a133 imu absent");
    assert_eq!(
        pf.acquire::<pocketforge::Imu>().err(),
        Some(pocketforge::CapError::HardwareAbsent),
        "broker: acquire(imu) hardware-absent over the wire"
    );
    assert_eq!(pf.backend().rumble_pulse(40), pocketforge::RumbleStatus::NoopAbsent);
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn broker_pose_round_trips_over_the_wire() {
    // Set then get the pose THROUGH the PFW1 socket and confirm it survives. Needs a device with a
    // BOUND IMU — no shipping device has one today (a523's qmi8658 is DT-present but driver-
    // unbound, so platform omits the row), so this drives the synthetic rig over the real wire.
    // It named the a523 until tsp-ozbp.16, which only held against the stale vendored copy.
    let (sock, _b) = start_ref_broker_for(common::imu_descriptor(), "synthimu");
    let pf = broker_pf_for(common::imu_descriptor(), &sock);
    let want = Pose { yaw: 12.5, pitch: -3.0, roll: 90.0, x: 1.0, y: 2.0, z: 3.0, wx: 0.1, wy: 0.2, wz: 0.3 };
    let set = pf.backend().set_pose(want).expect("set_pose over wire");
    assert_eq!(set, want);
    let got = pf.backend().get_pose().expect("get_pose over wire");
    assert_eq!(got, want, "pose did not survive the wire round-trip");
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn broker_cooperative_set_get_capability_round_trips() {
    // a523 settings (granted) — set a value through the broker, read it back.
    let (sock, _b) = start_ref_broker("a523");
    let pf = broker_pf("a523", &sock);
    pf.backend().set_capability("settings", b"brightness=42").expect("set over wire");
    let v = pf.backend().get_capability("settings").expect("get over wire");
    assert_eq!(v, b"brightness=42");
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn descriptor_loads_are_well_formed() {
    // Sanity: both PLATFORM descriptors parse off disk and expose their identity (cheap guard on
    // the loader, and on this repo still being able to read what platform ships).
    for id in ["a133", "a523"] {
        let path = pocketforge::test_support::try_descriptor_path(id).expect("descriptor path");
        let d = Descriptor::load(&path).unwrap();
        assert_eq!(d.identity.id, id);
        assert!(!d.inputs.is_empty());
    }
}
