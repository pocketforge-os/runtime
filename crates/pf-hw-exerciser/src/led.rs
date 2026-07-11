//! `led` — walk `/sys/class/leds`, blink each candidate in sequence with clear stdout markers so
//! the owner can call out which physical LED (thumbstick rings, etc.) lit for each node.

use crate::{has_flag, opt};
use std::io::Write;

const LED_ROOT: &str = "/sys/class/leds";

fn read_u64(path: &str) -> Option<u64> {
    std::fs::read_to_string(path).ok().and_then(|s| s.trim().parse().ok())
}

fn write_val(dir: &str, attr: &str, val: u64) -> std::io::Result<()> {
    std::fs::write(format!("{dir}/{attr}"), val.to_string())
}

fn leds() -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(LED_ROOT) else {
        return Vec::new();
    };
    let mut out: Vec<String> = rd
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    // Natural-ish sort so sunxi_led0,1,2,... come in order.
    out.sort_by_key(|n| natural(n));
    out
}

/// Split a name into (text, number) chunks for a natural sort.
fn natural(s: &str) -> Vec<(String, u64)> {
    let mut chunks = Vec::new();
    let mut txt = String::new();
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            if !txt.is_empty() && num.is_empty() {
                // starting a number after text
            }
            num.push(c);
        } else {
            if !num.is_empty() {
                chunks.push((std::mem::take(&mut txt), num.parse().unwrap_or(0)));
                num.clear();
            }
            txt.push(c);
        }
    }
    chunks.push((txt, num.parse().unwrap_or(0)));
    chunks
}

pub fn run(args: &[String]) -> i32 {
    let only = opt(args, "--only");
    let on_ms: u64 = opt(args, "--on-ms").and_then(|s| s.parse().ok()).unwrap_or(900);
    let gap_ms: u64 = opt(args, "--gap-ms").and_then(|s| s.parse().ok()).unwrap_or(400);
    let repeat: u32 = opt(args, "--repeat").and_then(|s| s.parse().ok()).unwrap_or(1);
    let list_only = has_flag(args, "--list");

    println!("== led exerciser (/sys/class/leds) ==");
    let all = leds();
    if all.is_empty() {
        eprintln!("FAIL: {LED_ROOT} empty or unreadable");
        return 4;
    }
    let selected: Vec<String> = all
        .iter()
        .filter(|n| only.map(|s| n.contains(s)).unwrap_or(true))
        .cloned()
        .collect();
    for n in &all {
        let dir = format!("{LED_ROOT}/{n}");
        let maxb = read_u64(&format!("{dir}/max_brightness")).unwrap_or(0);
        let cur = read_u64(&format!("{dir}/brightness")).unwrap_or(0);
        let sel = if selected.contains(n) { "*" } else { " " };
        println!("  {sel} {n:<20} max_brightness={maxb} brightness={cur}");
    }
    if list_only {
        return 0;
    }
    println!("blinking {} node(s), on={on_ms}ms gap={gap_ms}ms repeat={repeat}", selected.len());
    println!();

    for r in 1..=repeat {
        for (i, name) in selected.iter().enumerate() {
            let dir = format!("{LED_ROOT}/{name}");
            let maxb = read_u64(&format!("{dir}/max_brightness")).unwrap_or(1).max(1);
            let saved = read_u64(&format!("{dir}/brightness")).unwrap_or(0);
            print!("LED[{i}] round {r}/{repeat}  {name}  -> ON (brightness={maxb}) ... ");
            std::io::stdout().flush().ok();
            if let Err(e) = write_val(&dir, "brightness", maxb) {
                println!("WRITE_FAIL ({e})");
                continue;
            }
            std::thread::sleep(std::time::Duration::from_millis(on_ms));
            let _ = write_val(&dir, "brightness", saved);
            println!("OFF");
            std::thread::sleep(std::time::Duration::from_millis(gap_ms));
        }
    }
    println!();
    println!("PASS: blinked {} LED node(s) — OWNER maps each node index to a physical LED", selected.len());
    0
}
