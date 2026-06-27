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
/// `id`'s descriptor) on a background thread. Returns (socket path, the server's backend).
fn start_ref_broker(id: &str) -> (std::path::PathBuf, Arc<InProcessBackend>) {
    let n = SOCK_SEQ.fetch_add(1, Ordering::Relaxed);
    let sock = std::env::temp_dir().join(format!("pf-swap-{}-{}-{}.sock", id, std::process::id(), n));
    let _ = std::fs::remove_file(&sock);

    let backend = InProcessBackend::shared(Arc::new(common::descriptor(id)));
    let server_backend: Arc<dyn Backend> = backend.clone();
    let listener = UnixListener::bind(&sock).expect("bind ref-broker socket");
    std::thread::spawn(move || {
        let _ = server::serve(listener, server_backend);
    });
    (sock, backend)
}

/// Build a `Pf` over the out-of-process broker at `sock` for `id`'s descriptor.
fn broker_pf(id: &str, sock: &std::path::Path) -> Pf {
    let descriptor = Arc::new(common::descriptor(id));
    Pf::via_broker(descriptor, sock).expect("connect broker client")
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
    // a523 has an IMU; set then get the pose THROUGH the PFW1 socket and confirm it survives.
    let (sock, _b) = start_ref_broker("a523");
    let pf = broker_pf("a523", &sock);
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
    // Sanity: both fixtures parse and expose their identity (cheap guard on the loader).
    for id in ["a133", "a523"] {
        let d = Descriptor::load(common::fixture_path(id)).unwrap();
        assert_eq!(d.identity.id, id);
        assert!(!d.inputs.is_empty());
    }
}
