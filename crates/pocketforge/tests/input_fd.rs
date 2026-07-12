//! `pf_acquire_input_fd` / `Pf::acquire_input_fd` / `InputHandle::acquire_fd` — the input event
//! fd handoff (`tsp-e1b.10`), proven DEVICE-FREE over the v0 in-process backend.
//!
//! The core proof: the facade hands back a real, readable fd for the platform-provided input
//! node, and reading it yields the injected `EV_KEY`/`EV_ABS`/`EV_SYN` `input_event` records —
//! exactly what the E5 sim's synth `uinput` node exposes. A FIFO stands in for the node so the
//! test needs no `/dev/uinput`, no root, and no hardware (the real-uinput fidelity leg lives in
//! `pf-input-broker`). The gate is proven too: hardware-absent and consent-denied input return
//! the four-way taxonomy, never an ambient `/dev` open.

use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pocketforge::backends::InProcessBackend;
use pocketforge::{CapError, Descriptor, Input, PermissionState, Pf};

/// Canonical Linux codes we inject (avoid a `linux/input.h` dep).
const EV_KEY: u16 = 1;
const EV_SYN: u16 = 0;
const BTN_SOUTH: u16 = 0x130; // 304
const SYN_REPORT: u16 = 0;

/// One `struct input_event` as its 24 wire bytes on 64-bit Linux: `timeval`(16) + type(2) +
/// code(2) + value(4). We zero the time (the reader only asserts type/code/value).
fn event_bytes(ev_type: u16, code: u16, value: i32) -> [u8; 24] {
    let mut b = [0u8; 24];
    b[16..18].copy_from_slice(&ev_type.to_ne_bytes());
    b[18..20].copy_from_slice(&code.to_ne_bytes());
    b[20..24].copy_from_slice(&value.to_ne_bytes());
    b
}

fn a133_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/a133-capabilities.toml")
}

/// Make a unique FIFO path under the temp dir and `mkfifo` it.
fn make_fifo(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("pf_input_fd_{}_{}.fifo", tag, std::process::id()));
    let _ = std::fs::remove_file(&p);
    let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
    let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "mkfifo({}) failed: {}", p.display(), std::io::Error::last_os_error());
    p
}

/// Read `want` bytes off a non-blocking fd, retrying past `EAGAIN` up to ~2s.
fn read_exact_nonblock(fd: i32, want: usize) -> Vec<u8> {
    let mut got = Vec::with_capacity(want);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while got.len() < want && std::time::Instant::now() < deadline {
        let mut buf = [0u8; 256];
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n > 0 {
            got.extend_from_slice(&buf[..n as usize]);
        } else {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    got
}

/// The load-bearing device-free proof over the in-process backend: gate + a real readable fd +
/// injected events read back verbatim + the taxonomy on the failure paths. All in one test fn so
/// the one env-touching assertion is sequential (no cross-test `PF_INPUT_NODE` race).
#[test]
fn in_process_input_fd_reads_injected_events() {
    let desc = Arc::new(Descriptor::load(a133_fixture()).expect("load a133 fixture"));

    // --- (1) hardware-absent when no node is provided (input IS present, but nothing to open) ---
    std::env::remove_var("PF_INPUT_NODE");
    let no_node = Pf::over_in_process(Arc::new(InProcessBackend::new(desc.clone())));
    assert_eq!(
        no_node.acquire_input_fd().err(),
        Some(CapError::HardwareAbsent),
        "no platform node ⇒ HardwareAbsent, never a /dev scan"
    );

    // --- (2) consent-denied gate: the node is set, but policy blocks input ⇒ ConsentDenied ---
    let fifo_denied = make_fifo("denied");
    let denied_be = Arc::new(InProcessBackend::new(desc.clone()).with_input_node(&fifo_denied));
    denied_be.set_consent("input", PermissionState::Denied);
    let denied = Pf::over_in_process(denied_be);
    assert_eq!(
        denied.acquire_input_fd().err(),
        Some(CapError::ConsentDenied),
        "policy-blocked input ⇒ ConsentDenied (fd never opened)"
    );
    let _ = std::fs::remove_file(&fifo_denied);

    // --- (3) success: a real fd; reading it yields the injected records verbatim ---
    let fifo = make_fifo("ok");
    let be = Arc::new(InProcessBackend::new(desc.clone()).with_input_node(&fifo));
    let pf = Pf::over_in_process(be);

    // The read fd (O_NONBLOCK) opens without a writer present.
    let fd = pf.acquire_input_fd().expect("acquire_input_fd returns an fd");
    // Open the write end and inject a BTN_SOUTH press + SYN report.
    let mut w = std::fs::OpenOptions::new().write(true).open(&fifo).expect("open FIFO writer");
    let press = event_bytes(EV_KEY, BTN_SOUTH, 1);
    let syn = event_bytes(EV_SYN, SYN_REPORT, 0);
    w.write_all(&press).unwrap();
    w.write_all(&syn).unwrap();
    w.flush().unwrap();

    let got = read_exact_nonblock(fd.as_raw_fd(), 48);
    assert_eq!(got.len(), 48, "read back both 24-byte records");
    assert_eq!(&got[0..24], &press, "first record is the BTN_SOUTH press verbatim");
    assert_eq!(&got[24..48], &syn, "second record is the SYN report verbatim");
    drop(w);
    drop(fd);
    let _ = std::fs::remove_file(&fifo);

    // --- (4) hardware-absent when the descriptor has NO input rows (gate fires before any open) ---
    let empty = Arc::new(
        Descriptor::from_toml(
            "[identity]\nid=\"empty\"\nmanufacturer=\"x\"\nmodel=\"y\"\nsdl_guid=\"0\"\n",
        )
        .expect("parse empty descriptor"),
    );
    let fifo2 = make_fifo("empty");
    let empty_pf =
        Pf::over_in_process(Arc::new(InProcessBackend::new(empty).with_input_node(&fifo2)));
    assert_eq!(
        empty_pf.acquire_input_fd().err(),
        Some(CapError::HardwareAbsent),
        "no input rows ⇒ HardwareAbsent even with a node set"
    );
    let _ = std::fs::remove_file(&fifo2);
}

/// The same fd off an already-acquired `InputHandle` (the typed-facade path apps use), proving
/// `handle.acquire_fd()` and `Pf::acquire_input_fd()` are one seam. Uses an explicit node, so it
/// never touches `PF_INPUT_NODE` and is parallel-safe alongside the env-touching test above.
#[test]
fn input_handle_acquire_fd_matches_facade() {
    let desc = Arc::new(Descriptor::load(a133_fixture()).expect("load a133 fixture"));
    let fifo = make_fifo("handle");
    let pf = Pf::over_in_process(Arc::new(InProcessBackend::new(desc).with_input_node(&fifo)));

    let handle = pf.acquire::<Input>().expect("acquire the Input handle");
    let fd = handle.acquire_fd().expect("handle vends the input fd");

    let mut w = std::fs::OpenOptions::new().write(true).open(&fifo).expect("open FIFO writer");
    let ev = event_bytes(EV_KEY, BTN_SOUTH, 1);
    w.write_all(&ev).unwrap();
    w.flush().unwrap();

    let got = read_exact_nonblock(fd.as_raw_fd(), 24);
    assert_eq!(got, ev, "reading the handle's fd yields the injected record");
    drop(w);
    let _ = std::fs::remove_file(&fifo);
}
