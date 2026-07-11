//! `rumble` — find the `FF_RUMBLE`-capable evdev node, upload an effect, and play it. The owner
//! physically confirms the motor fires (relayed via the coordinator).

use crate::evdev;
use crate::{opt, has_flag};
use libc::{c_void, ff_effect, input_event};
use std::os::unix::io::AsRawFd;

const EV_FF: u16 = 0x15;
const FF_RUMBLE: u16 = 0x50;

/// Does `fd` advertise `FF_RUMBLE`?
fn has_ff_rumble(fd: i32) -> bool {
    // FF codes are < 0x80 ⇒ 16 bytes of bitmask is ample.
    let mut bits = [0u8; 16];
    let r = unsafe { libc::ioctl(fd, evdev::eviocgbit(EV_FF, bits.len()), bits.as_mut_ptr()) };
    r >= 0 && evdev::test_bit(&bits, FF_RUMBLE)
}

/// Upload a rumble effect; returns the kernel-assigned effect id.
fn upload(fd: i32, strong: u16, weak: u16, ms: u16) -> Result<i16, String> {
    let mut eff: ff_effect = unsafe { std::mem::zeroed() };
    // `ff_effect.type` is the EFFECT type (FF_RUMBLE), NOT the EV_FF event type — the kernel's
    // input_ff_upload() rejects anything else with EINVAL (verified on real A523 pwm-vibrator
    // silicon: setting EV_FF here returned `EVIOCSFF: Invalid argument`). EV_FF is only the
    // `type` of the PLAY input_event written in set_play().
    eff.type_ = FF_RUMBLE;
    eff.id = -1; // -1 ⇒ kernel assigns a fresh id
    eff.direction = 0;
    eff.replay.length = ms;
    eff.replay.delay = 0;
    // libc models the ff_effect union as `u: [u64; 4]`. For FF_RUMBLE the union head is
    // `struct ff_rumble_effect { u16 strong_magnitude; u16 weak_magnitude; }` — little-endian:
    // strong in bits 0..16, weak in bits 16..32 of the first u64.
    eff.u[0] = (strong as u64) | ((weak as u64) << 16);
    let r = unsafe { libc::ioctl(fd, evdev::eviocsff(), &mut eff as *mut ff_effect) };
    if r < 0 {
        return Err(format!("EVIOCSFF failed: {}", std::io::Error::last_os_error()));
    }
    Ok(eff.id)
}

/// Write a play/stop gain event for effect `id`.
fn set_play(fd: i32, id: i16, on: bool) -> Result<(), String> {
    let mut ev: input_event = unsafe { std::mem::zeroed() };
    ev.type_ = EV_FF;
    ev.code = id as u16;
    ev.value = if on { 1 } else { 0 };
    let n = unsafe {
        libc::write(fd, &ev as *const input_event as *const c_void, std::mem::size_of::<input_event>())
    };
    if n < 0 {
        return Err(format!("play write failed: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

fn remove(fd: i32, id: i16) {
    let idv = id as libc::c_int;
    unsafe {
        libc::ioctl(fd, evdev::eviocrmff(), idv as libc::c_ulong);
    }
}

/// Enumerate `/dev/input/event*`, opening each read-write, returning `(path, name, has_ff)`.
fn scan() -> Vec<(String, String, bool)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/dev/input") else {
        return out;
    };
    let mut entries: Vec<String> = rd
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("event"))
        .collect();
    entries.sort_by_key(|n| n[5..].parse::<u32>().unwrap_or(u32::MAX));
    for name in entries {
        let path = format!("/dev/input/{name}");
        let f = match std::fs::OpenOptions::new().read(true).write(true).open(&path) {
            Ok(f) => f,
            Err(_) => {
                // fall back to read-only just to read the name for the inventory
                match std::fs::File::open(&path) {
                    Ok(f) => {
                        let nm = evdev::device_name(f.as_raw_fd());
                        out.push((path, nm, false));
                        continue;
                    }
                    Err(_) => continue,
                }
            }
        };
        let fd = f.as_raw_fd();
        let nm = evdev::device_name(fd);
        let ff = has_ff_rumble(fd);
        out.push((path, nm, ff));
    }
    out
}

pub fn run(args: &[String]) -> i32 {
    let strong: u16 = opt(args, "--strong").and_then(|s| s.parse().ok()).unwrap_or(0xFFFF);
    let weak: u16 = opt(args, "--weak").and_then(|s| s.parse().ok()).unwrap_or(0xFFFF);
    let ms: u16 = opt(args, "--ms").and_then(|s| s.parse().ok()).unwrap_or(500);
    let count: u32 = opt(args, "--count").and_then(|s| s.parse().ok()).unwrap_or(3);
    let gap_ms: u64 = opt(args, "--gap").and_then(|s| s.parse().ok()).unwrap_or(700);
    let list = has_flag(args, "--list");

    println!("== rumble exerciser (FF_RUMBLE) ==");
    let nodes = scan();
    for (p, nm, ff) in &nodes {
        println!("  node {p:<22} name={nm:?} ff_rumble={ff}");
    }
    if list {
        return 0;
    }

    // Pick the node: explicit --node, else the first FF_RUMBLE-capable one.
    let chosen = match opt(args, "--node") {
        Some(n) => n.to_string(),
        None => match nodes.iter().find(|(_, _, ff)| *ff) {
            Some((p, _, _)) => p.clone(),
            None => {
                eprintln!("FAIL: no FF_RUMBLE-capable /dev/input/event* node found");
                return 3;
            }
        },
    };
    println!("chosen_node={chosen}");

    let f = match std::fs::OpenOptions::new().read(true).write(true).open(&chosen) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("FAIL: open {chosen} rw: {e} (need root)");
            return 3;
        }
    };
    let fd = f.as_raw_fd();
    if !has_ff_rumble(fd) {
        eprintln!("WARN: {chosen} does not advertise FF_RUMBLE — playing anyway (explicit --node)");
    }

    let id = match upload(fd, strong, weak, ms) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("FAIL: {e}");
            return 3;
        }
    };
    println!("uploaded effect id={id} strong=0x{strong:04X} weak=0x{weak:04X} length_ms={ms}");

    for i in 1..=count {
        print!("PLAY {i}/{count} (id={id}) ... ");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        if let Err(e) = set_play(fd, id, true) {
            eprintln!("\nFAIL: {e}");
            remove(fd, id);
            return 3;
        }
        std::thread::sleep(std::time::Duration::from_millis(ms as u64 + 50));
        set_play(fd, id, false).ok();
        println!("done");
        if i < count {
            std::thread::sleep(std::time::Duration::from_millis(gap_ms));
        }
    }
    remove(fd, id);
    println!("PASS: rumble sequence played on {chosen} — OWNER must confirm the motor fired");
    0
}
