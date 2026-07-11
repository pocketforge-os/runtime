//! `imu` — dump accel+gyro from the real IIO node at ~50 Hz, printing RAW, chip-frame SI, and the
//! mount-matrix-TRANSFORMED device frame side by side. The transform uses OUR shared math
//! ([`pocketforge::physical_model::apply_mount`]) — the same `apply_mount` the `.4`
//! `SensorManager::device_from_chip` calls — so this exercises OUR pipeline on real silicon, not
//! an ad-hoc reimplementation.
//!
//! Device-frame convention (from `physical_model`): X=right, Y=up (screen top), Z=out of screen.
//! FLAT face-up at rest ⇒ device-frame accel ≈ (0, 0, +9.80665). That flat-test is the
//! mount-matrix verdict: if gravity lands on device Z with the descriptor matrix (identity today),
//! the descriptor is CONFIRMED; if it lands on another axis/sign, we propose the correcting matrix.

use crate::opt;
use pocketforge::physical_model::{self, Mat3, IDENTITY_MOUNT};

const IIO_ROOT: &str = "/sys/bus/iio/devices";

fn read_str(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn read_f64(path: &str) -> Option<f64> {
    read_str(path).and_then(|s| s.parse().ok())
}

/// An IIO channel group (accel or gyro) resolved to concrete sysfs paths + scale/offset.
struct Chan {
    dev: String,      // iio:deviceN dir
    prefix: String,   // "in_accel" | "in_anglvel"
    scale: [f64; 3],  // per-axis scale (shared scale broadcast to all 3)
    offset: [f64; 3],
}

impl Chan {
    /// Resolve a channel group under `dev` if `<dev>/<prefix>_x_raw` exists.
    fn resolve(dev: &str, prefix: &str) -> Option<Chan> {
        let x_raw = format!("{dev}/{prefix}_x_raw");
        if !std::path::Path::new(&x_raw).exists() {
            return None;
        }
        let axis_scale = |ax: char| {
            read_f64(&format!("{dev}/{prefix}_{ax}_scale"))
                .or_else(|| read_f64(&format!("{dev}/{prefix}_scale")))
                .unwrap_or(1.0)
        };
        let axis_off = |ax: char| {
            read_f64(&format!("{dev}/{prefix}_{ax}_offset"))
                .or_else(|| read_f64(&format!("{dev}/{prefix}_offset")))
                .unwrap_or(0.0)
        };
        Some(Chan {
            dev: dev.to_string(),
            prefix: prefix.to_string(),
            scale: [axis_scale('x'), axis_scale('y'), axis_scale('z')],
            offset: [axis_off('x'), axis_off('y'), axis_off('z')],
        })
    }

    /// Raw counts [x,y,z].
    fn raw(&self) -> Option<[f64; 3]> {
        let mut v = [0.0; 3];
        for (i, ax) in ['x', 'y', 'z'].iter().enumerate() {
            v[i] = read_f64(&format!("{}/{}_{}_raw", self.dev, self.prefix, ax))?;
        }
        Some(v)
    }

    /// Chip-frame SI: (raw + offset) * scale — the IIO convention (accel⇒m/s², anglvel⇒rad/s).
    fn si(&self, raw: &[f64; 3]) -> [f64; 3] {
        let mut v = [0.0; 3];
        for i in 0..3 {
            v[i] = (raw[i] + self.offset[i]) * self.scale[i];
        }
        v
    }
}

/// Enumerate `iio:deviceN` dirs with their `name`.
fn iio_devices() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(IIO_ROOT) else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name().into_string().unwrap_or_default();
        if name.starts_with("iio:device") {
            let dir = format!("{IIO_ROOT}/{name}");
            let dev_name = read_str(&format!("{dir}/name")).unwrap_or_default();
            out.push((dir, dev_name));
        }
    }
    out.sort();
    out
}

fn parse_mount(s: &str) -> Option<Mat3> {
    let nums: Vec<f64> = s.split(',').filter_map(|t| t.trim().parse().ok()).collect();
    if nums.len() != 9 {
        return None;
    }
    Some([
        [nums[0], nums[1], nums[2]],
        [nums[3], nums[4], nums[5]],
        [nums[6], nums[7], nums[8]],
    ])
}

pub fn run(args: &[String]) -> i32 {
    let secs: f64 = opt(args, "--secs").and_then(|s| s.parse().ok()).unwrap_or(4.0);
    let hz: f64 = opt(args, "--hz").and_then(|s| s.parse().ok()).unwrap_or(50.0);
    let mount: Mat3 = match opt(args, "--mount") {
        Some(s) => match parse_mount(s) {
            Some(m) => m,
            None => {
                eprintln!("FAIL: --mount needs 9 comma-separated numbers");
                return 2;
            }
        },
        None => IDENTITY_MOUNT,
    };

    println!("== imu exerciser (IIO accel+gyro) ==");
    let devs = iio_devices();
    if devs.is_empty() {
        println!("IIO_BIND: NONE — {IIO_ROOT} is EMPTY. qmi8658/mmc5603 did NOT bind as IIO on stock.");
        println!("(This is the SPIKE-0 sensor-bind finding: DT-present but driver-unbound.)");
        return 4;
    }
    println!("IIO devices present:");
    for (dir, name) in &devs {
        println!("  {dir}  name={name:?}");
    }
    // Bind findings for the SPIKE-0 cross-post.
    let bound = |needle: &str| devs.iter().any(|(_, n)| n.contains(needle));
    println!(
        "IIO_BIND: qmi8658={} mmc5603={}",
        if bound("qmi8658") { "BOUND" } else { "absent" },
        if bound("mmc5603") { "BOUND" } else { "absent" },
    );

    // Resolve accel + gyro channel groups (may live on the same iio device — qmi8658 does both).
    let accel = devs.iter().find_map(|(d, _)| Chan::resolve(d, "in_accel"));
    let gyro = devs.iter().find_map(|(d, _)| Chan::resolve(d, "in_anglvel"));
    if accel.is_none() && gyro.is_none() {
        eprintln!("FAIL: no in_accel_*_raw or in_anglvel_*_raw channels on any IIO device");
        return 4;
    }
    if let Some(a) = &accel {
        println!("accel: {}/{}_*  scale={:?} offset={:?}", a.dev, a.prefix, a.scale, a.offset);
    }
    if let Some(g) = &gyro {
        println!("gyro:  {}/{}_*  scale={:?} offset={:?}", g.dev, g.prefix, g.scale, g.offset);
    }
    println!("mount_matrix (chip->device, M·chip) = {mount:?}");
    println!();
    println!(
        "{:>6} | {:>26} | {:>26} | {:>26}",
        "kind", "raw[x,y,z]", "chip-SI[x,y,z]", "device[x,y,z] (M·chip)"
    );

    let period = std::time::Duration::from_secs_f64(1.0 / hz.max(1.0));
    let n = (secs * hz).round().max(1.0) as u64;
    let fmt = |v: &[f64; 3]| format!("{:8.3},{:8.3},{:8.3}", v[0], v[1], v[2]);
    // Running mean of the device-frame accel for the flat-test verdict.
    let mut acc_sum = [0.0f64; 3];
    let mut acc_n = 0u64;
    for _ in 0..n {
        if let Some(a) = &accel {
            if let Some(raw) = a.raw() {
                let chip = a.si(&raw);
                let dev = physical_model::apply_mount(&mount, &chip);
                for i in 0..3 {
                    acc_sum[i] += dev[i];
                }
                acc_n += 1;
                println!(" accel | {:>26} | {:>26} | {:>26}", fmt(&raw), fmt(&chip), fmt(&dev));
            }
        }
        if let Some(g) = &gyro {
            if let Some(raw) = g.raw() {
                let chip = g.si(&raw);
                let dev = physical_model::apply_mount(&mount, &chip);
                println!("  gyro | {:>26} | {:>26} | {:>26}", fmt(&raw), fmt(&chip), fmt(&dev));
            }
        }
        std::thread::sleep(period);
    }

    if acc_n > 0 {
        let mean = [acc_sum[0] / acc_n as f64, acc_sum[1] / acc_n as f64, acc_sum[2] / acc_n as f64];
        let (axis, val) = mean
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
            .map(|(i, v)| (["X", "Y", "Z"][i], *v))
            .unwrap();
        println!();
        println!("FLAT-TEST device-frame accel mean = {}", fmt(&mean));
        println!(
            "  gravity dominant on device {axis} = {val:+.3} m/s^2 (expect +Z≈+9.807 flat face-up)"
        );
        let ok = axis == "Z" && (val - physical_model::G).abs() < 2.5;
        println!(
            "  MOUNT-MATRIX VERDICT: {}",
            if ok {
                "CONSISTENT with the descriptor matrix under flat-test (owner tilt-test confirms axes)"
            } else {
                "DIVERGES — gravity not on +Z; propose a correcting mount_matrix (hand to SPIKE-0)"
            }
        );
    }
    0
}
