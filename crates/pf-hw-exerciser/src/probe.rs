//! `probe` — a device-free-of-owner inventory: enumerate input/IIO/LED nodes and report whether the
//! A523 sensors (`qmi8658`, `mmc5603`) actually BIND as IIO on stock. No committed A523 node table
//! exists yet (the 2026-06-19 probe found `/sys/bus/iio/devices` EMPTY) — this establishes it and
//! its `qmi8658/mmc5603` bind line is cross-posted to `tsp-9sx.1` (SPIKE-0).

use crate::evdev;
use std::os::unix::io::AsRawFd;

const EV_FF: u16 = 0x15;
const FF_RUMBLE: u16 = 0x50;

fn has_ff_rumble(fd: i32) -> bool {
    let mut bits = [0u8; 16];
    let r = unsafe { libc::ioctl(fd, evdev::eviocgbit(EV_FF, bits.len()), bits.as_mut_ptr()) };
    r >= 0 && evdev::test_bit(&bits, FF_RUMBLE)
}

fn read_str(path: &str) -> String {
    std::fs::read_to_string(path).map(|s| s.trim().to_string()).unwrap_or_default()
}

pub fn run(_args: &[String]) -> i32 {
    println!("== A523 peripheral node inventory (stock) ==");

    // --- /dev/input/event* ---
    println!("\n[/dev/input]");
    let mut ff_nodes = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/dev/input") {
        let mut names: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("event"))
            .collect();
        names.sort_by_key(|n| n[5..].parse::<u32>().unwrap_or(u32::MAX));
        for n in names {
            let path = format!("/dev/input/{n}");
            match std::fs::File::open(&path) {
                Ok(f) => {
                    let fd = f.as_raw_fd();
                    let nm = evdev::device_name(fd);
                    let ff = has_ff_rumble(fd);
                    if ff {
                        ff_nodes.push(path.clone());
                    }
                    println!("  {path:<22} name={nm:?} ff_rumble={ff}");
                }
                Err(e) => println!("  {path:<22} (open failed: {e})"),
            }
        }
    } else {
        println!("  (no /dev/input)");
    }
    println!("  FF_RUMBLE nodes: {ff_nodes:?}");

    // --- /sys/bus/iio/devices ---
    println!("\n[/sys/bus/iio/devices]");
    let mut iio_names = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/sys/bus/iio/devices") {
        let mut devs: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("iio:device"))
            .collect();
        devs.sort();
        if devs.is_empty() {
            println!("  EMPTY — no IIO devices bound.");
        }
        for d in devs {
            let dir = format!("/sys/bus/iio/devices/{d}");
            let name = read_str(&format!("{dir}/name"));
            iio_names.push(name.clone());
            // list the channel attrs that matter
            let chans: Vec<String> = std::fs::read_dir(&dir)
                .map(|rd| {
                    rd.flatten()
                        .filter_map(|e| e.file_name().into_string().ok())
                        .filter(|f| f.contains("_raw") || f.contains("_scale"))
                        .collect()
                })
                .unwrap_or_default();
            println!("  {dir}  name={name:?}");
            let mut cs = chans;
            cs.sort();
            for c in cs {
                println!("      {c}");
            }
        }
    } else {
        println!("  (no /sys/bus/iio/devices)");
    }
    let bound = |needle: &str| iio_names.iter().any(|n| n.contains(needle));
    println!(
        "  SENSOR-BIND (SPIKE-0): qmi8658={} mmc5603={}",
        if bound("qmi8658") { "BOUND" } else { "absent" },
        if bound("mmc5603") { "BOUND" } else { "absent" },
    );

    // --- /sys/class/leds ---
    println!("\n[/sys/class/leds]");
    if let Ok(rd) = std::fs::read_dir("/sys/class/leds") {
        let mut names: Vec<String> =
            rd.flatten().filter_map(|e| e.file_name().into_string().ok()).collect();
        names.sort();
        println!("  count={}", names.len());
        for n in &names {
            let maxb = read_str(&format!("/sys/class/leds/{n}/max_brightness"));
            println!("  {n:<20} max_brightness={maxb}");
        }
    } else {
        println!("  (no /sys/class/leds)");
    }

    println!("\n== inventory complete ==");
    0
}
