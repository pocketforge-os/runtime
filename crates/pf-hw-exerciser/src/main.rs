//! `pf-hw-exerciser` — device-facing hardware ground-truth exercisers for the A523
//! (TrimUI Smart Pro S), the HARDWARE GATE of `tsp-e1b.4`.
//!
//! The `.4` per-capability managers (vibration / sensors + `physical_model`) were verified only
//! against the E5 sim. This tool runs on the REAL device (over SSH, stock vendor 5.15.147) and
//! drives the actual silicon:
//!
//! * `probe` — inventory `/dev/input/event*`, `/sys/bus/iio/devices`, `/sys/class/leds`; report
//!   whether `qmi8658` (IMU) and `mmc5603` (mag) actually BIND (cross-feeds SPIKE-0).
//! * `rumble` — find the `FF_RUMBLE`-capable evdev node, upload + play effects (the owner feels it).
//! * `imu` — dump accel+gyro at ~50 Hz, RAW vs mount-matrix-TRANSFORMED, reusing OUR
//!   [`pocketforge::physical_model`] `apply_mount` so the test exercises OUR math.
//! * `led` — walk `/sys/class/leds`, blink each candidate in sequence (owner calls out which lit).
//!
//! Built fully static (`aarch64-unknown-linux-musl`) because the stock userland is BusyBox and we
//! assume nothing about its libc. NOT part of the shipped runtime — an `itest`-tier exerciser crate.

use std::env;
use std::process::exit;

mod evdev;
mod imu;
mod led;
mod probe;
mod rumble;

fn main() {
    let args: Vec<String> = env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("");
    let rest: &[String] = if args.len() > 2 { &args[2..] } else { &[] };
    let code = match cmd {
        "probe" => probe::run(rest),
        "rumble" => rumble::run(rest),
        "imu" => imu::run(rest),
        "led" => led::run(rest),
        "-h" | "--help" | "help" | "" => {
            usage();
            0
        }
        other => {
            eprintln!("pf-hw-exerciser: unknown subcommand {other:?}");
            usage();
            2
        }
    };
    exit(code);
}

fn usage() {
    eprintln!(
        "pf-hw-exerciser <subcommand> [opts]\n\
         \n\
         probe   Inventory input/IIO/LED nodes; report qmi8658/mmc5603 bind (SPIKE-0).\n\
         rumble  [--node N] [--strong M] [--weak M] [--ms MS] [--count C] [--gap MS] [--list]\n\
         imu     [--secs S] [--hz HZ] [--mount a,b,c,d,e,f,g,h,i]\n\
         led     [--only SUBSTR] [--on-ms MS] [--gap-ms MS] [--repeat N] [--list]\n"
    );
}

/// Tiny `--flag value` parser shared by the subcommands (no clap — keep the static binary lean).
pub fn opt<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// Presence of a bare `--flag` (no value).
pub fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}
