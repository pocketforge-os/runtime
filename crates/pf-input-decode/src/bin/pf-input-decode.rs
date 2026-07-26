//! `pf-input-decode` — the A133 gamepad MCU decoder daemon (`tsp-ozbp.9`).
//!
//! Reads the two RX-only UARTs the pad MCU streams (`ttyS3` = right, `ttyS4` = left) at 19200
//! 8N1, decodes the 8-byte frames, and emits ONE standard evdev gamepad via `/dev/uinput`. Runs
//! until `SIGINT`/`SIGTERM`, then tears the virtual device down cleanly.
//!
//! Usage:
//!   pf-input-decode [--right /dev/ttyS3] [--left /dev/ttyS4]
//!
//! Requires read access to both ttys and `/dev/uinput` (the shipped systemd unit runs it as root).

use std::io::Read;
use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pf_input_decode::{a133_spec, FrameScanner, Side, SideDecoder, Uinput};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn arg(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

/// Read `dev` forever, decode its frames for `side`, and emit onto the shared uinput device.
/// Returns on stop or on an unrecoverable read/open error (logged).
fn pump(dev: &str, side: Side, uinput: Arc<Mutex<Uinput>>) {
    let mut port = match pf_input_decode::serial::open_19200_8n1(dev) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("pf-input-decode: open {dev} failed: {e}");
            STOP.store(true, Ordering::Relaxed);
            return;
        }
    };
    let mut scanner = FrameScanner::new();
    let mut decoder = SideDecoder::new(side);
    let mut buf = [0u8; 256];

    while !STOP.load(Ordering::Acquire) {
        let n = match port.read(&mut buf) {
            Ok(0) => continue, // VTIME timeout with no bytes — re-check the stop flag.
            Ok(n) => n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("pf-input-decode: read {dev} failed: {e}");
                STOP.store(true, Ordering::Relaxed);
                return;
            }
        };
        for frame in scanner.push(&buf[..n]) {
            let evs = decoder.apply(frame);
            if evs.is_empty() {
                continue;
            }
            let dev = uinput.lock().unwrap();
            for ev in &evs {
                if let Err(e) = dev.emit(ev.ev_type, ev.code, ev.value) {
                    eprintln!("pf-input-decode: emit failed: {e}");
                    STOP.store(true, Ordering::Relaxed);
                    return;
                }
            }
            let _ = dev.syn();
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let right = arg(&args, "--right").unwrap_or_else(|| "/dev/ttyS3".to_string());
    let left = arg(&args, "--left").unwrap_or_else(|| "/dev/ttyS4".to_string());

    let uinput = Uinput::create(&a133_spec())?;
    let node = uinput.node().unwrap_or("<unresolved>").to_string();

    // SAFETY: installing a simple handler that only sets an atomic flag. Cast via a fn pointer
    // (not a fn item) so the conversion to sighandler_t is explicit, not a numeric cast.
    let handler = on_signal as extern "C" fn(c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }

    eprintln!("pf-input-decode: right={right} left={left} node={node}");
    println!("node={node}");
    println!("ready");
    use std::io::Write;
    std::io::stdout().flush().ok();

    let uinput = Arc::new(Mutex::new(uinput));
    let (ur, ul) = (Arc::clone(&uinput), Arc::clone(&uinput));
    let hr = std::thread::spawn(move || pump(&right, Side::Right, ur));
    let hl = std::thread::spawn(move || pump(&left, Side::Left, ul));

    let _ = hr.join();
    let _ = hl.join();
    // The Arc's last owner drops the Uinput → UI_DEV_DESTROY. Guaranteed here since both
    // threads have joined and released their clones.
    Ok(())
}
