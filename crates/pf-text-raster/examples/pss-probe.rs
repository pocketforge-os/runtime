//! tsp-ihne1.12 PSS evidence probe.
//!
//! Builds the production default font set, measures a fixed corpus in two phases, and samples
//! the process residency from /proc/self/smaps_rollup every iteration (peak and final):
//!
//! 1. Latin-only phase — the memory-mapped Noto Sans CJK collection must stay mostly
//!    NON-RESIDENT while no CJK text is shaped (its pages are demand-paged from the shared
//!    file mapping and reclaimable, never copied onto the heap);
//! 2. CJK phase — shaping the CJK corpus touches only the pages it needs, reported from
//!    /proc/self/smaps as the resident size of the CJK file's mappings.
//!
//! The same source compiles against any post-tsp-ihne1.12 commit (disk-backed, memory-mapped,
//! cached system), so base-vs-head runs are directly comparable; measure_checksum must be
//! identical across the two (deterministic recipe preserved).
//!
//! Run: cargo run --release -p pf-text-raster --features default-fonts --example pss-probe

fn main() {
    run();
}

#[cfg(feature = "default-fonts")]
fn run() {
    use pf_text_raster::{measure, pocketforge_default_fonts, ResolvedTextStyle};

    const LATIN_CORPUS: [&str; 3] = [
        "Settings",
        "Brightness and display",
        "Wi-Fi networks and Bluetooth accessories",
    ];
    const CJK_CORPUS: &str = "\u{518d}\u{958b} \u{65e5}\u{672c}\u{8a9e}";
    const CJK_FACE: &str = "NotoSansCJK-Regular.ttc";
    const ITERATIONS: usize = 500;

    let style = ResolvedTextStyle {
        family: "Manrope".to_owned(),
        size_px: 16.0,
        weight: 400,
        line_height: 1.5,
        tracking_em: 0.0,
    };

    let fonts = pocketforge_default_fonts();
    assert!(
        fonts
            .source_paths()
            .iter()
            .flatten()
            .any(|path| path.file_name().and_then(|n| n.to_str()) == Some(CJK_FACE)),
        "the CJK face must be part of the production font set"
    );
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

    // Phase 1: Latin only.
    for iteration in 0..ITERATIONS {
        step(LATIN_CORPUS[iteration % LATIN_CORPUS.len()], &mut checksum);
    }
    let (latin_rss, latin_pss) = sample_residency();
    let latin_cjk_map_rss = mapped_file_rss_kb(CJK_FACE);

    // Phase 2: the CJK corpus.
    for _ in 0..ITERATIONS {
        step(CJK_CORPUS, &mut checksum);
    }
    let (final_rss, final_pss) = sample_residency();
    let final_cjk_map_rss = mapped_file_rss_kb(CJK_FACE);

    let exe = std::env::current_exe().expect("current exe path");
    let exe_bytes = std::fs::metadata(&exe).expect("exe metadata").len();
    println!("exe={}", exe.display());
    println!("exe_bytes={exe_bytes}");
    println!("measure_checksum={checksum:x}");
    println!("iterations={ITERATIONS}");
    println!("latin_phase_final_rss_kb={latin_rss}");
    println!("latin_phase_final_pss_kb={latin_pss}");
    println!("latin_phase_cjk_map_rss_kb={latin_cjk_map_rss}");
    println!("peak_rss_kb={peak_rss}");
    println!("peak_pss_kb={peak_pss}");
    println!("final_rss_kb={final_rss}");
    println!("final_pss_kb={final_pss}");
    println!("final_cjk_map_rss_kb={final_cjk_map_rss}");
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

/// Total resident size (kB) of every /proc/self/smaps mapping backed by `file_name`.
#[cfg(feature = "default-fonts")]
fn mapped_file_rss_kb(file_name: &str) -> u64 {
    let smaps = std::fs::read_to_string("/proc/self/smaps").expect("smaps");
    let mut total = 0u64;
    let mut in_target = false;
    for line in smaps.lines() {
        // Mapping headers start with a hex address range; field lines start with a keyword.
        if line.split_once([' ', '\t']).is_some_and(|(first, _)| {
            !first.is_empty() && first.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
        }) {
            in_target = line.contains(file_name);
        } else if in_target {
            if let Some(value) = line.strip_prefix("Rss:") {
                total += parse_kb(value);
            }
        }
    }
    total
}

#[cfg(feature = "default-fonts")]
fn parse_kb(field: &str) -> u64 {
    field
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .expect("smaps value parses")
}

#[cfg(not(feature = "default-fonts"))]
fn run() {
    eprintln!("build with --features default-fonts");
    std::process::exit(2);
}
