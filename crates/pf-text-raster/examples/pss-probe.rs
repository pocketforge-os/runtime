//! tsp-ihne1.12 PSS evidence probe.
//!
//! Builds the production default font set, measures a fixed corpus, and samples the process
//! residency from /proc/self/smaps_rollup every iteration (peak and final). The same source
//! compiles against the base commit (embedded fonts, per-measurement copy) and the head commit
//! (disk-backed, memory-mapped, cached system), so the two runs are directly comparable.
//!
//! Run: cargo run --release -p pf-text-raster --features default-fonts --example pss-probe

fn main() {
    run();
}

#[cfg(feature = "default-fonts")]
fn run() {
    use pf_text_raster::{measure, pocketforge_default_fonts, ResolvedTextStyle};

    const CORPUS: [&str; 4] = [
        "Settings",
        "Brightness and display",
        "Wi-Fi networks and Bluetooth accessories",
        "再開 日本語",
    ];
    const ITERATIONS: usize = 500;

    let style = ResolvedTextStyle {
        family: "Manrope".to_owned(),
        size_px: 16.0,
        weight: 400,
        line_height: 1.5,
        tracking_em: 0.0,
    };

    let fonts = pocketforge_default_fonts();
    let (mut peak_rss, mut peak_pss) = sample_residency();
    let mut checksum: u64 = 0;
    let mut step = |text: &str, checksum: &mut u64| {
        let (width, height) = measure(&fonts, &style, text, 1.0, Some(240.0));
        *checksum = checksum
            .wrapping_mul(31)
            .wrapping_add(width.to_bits() as u64)
            .wrapping_add(height.to_bits() as u64);
        let (rss, pss) = sample_residency();
        peak_rss = peak_rss.max(rss);
        peak_pss = peak_pss.max(pss);
    };
    for text in CORPUS {
        step(text, &mut checksum);
    }
    for iteration in 0..ITERATIONS {
        step(CORPUS[iteration % CORPUS.len()], &mut checksum);
    }
    let (final_rss, final_pss) = sample_residency();

    let exe = std::env::current_exe().expect("current exe path");
    let exe_bytes = std::fs::metadata(&exe).expect("exe metadata").len();
    println!("exe={}", exe.display());
    println!("exe_bytes={exe_bytes}");
    println!("measure_checksum={checksum:x}");
    println!("iterations={ITERATIONS}");
    println!("peak_rss_kb={peak_rss}");
    println!("peak_pss_kb={peak_pss}");
    println!("final_rss_kb={final_rss}");
    println!("final_pss_kb={final_pss}");
}

#[cfg(feature = "default-fonts")]
fn sample_residency() -> (u64, u64) {
    let rollup = std::fs::read_to_string("/proc/self/smaps_rollup").expect("smaps_rollup");
    let mut rss = 0;
    let mut pss = 0;
    for line in rollup.lines() {
        if let Some(value) = line.strip_prefix("Rss:") {
            rss = parse_kb(value);
        } else if let Some(value) = line.strip_prefix("Pss:") {
            pss = parse_kb(value);
        }
    }
    (rss, pss)
}

#[cfg(feature = "default-fonts")]
fn parse_kb(field: &str) -> u64 {
    field
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .expect("smaps_rollup value parses")
}

#[cfg(not(feature = "default-fonts"))]
fn run() {
    eprintln!("build with --features default-fonts");
    std::process::exit(2);
}
