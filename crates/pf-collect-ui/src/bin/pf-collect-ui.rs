//! `pf-collect-ui` — the on-panel guided-collection wizard (tsp-bwrg.3).
//!
//! Renders the REAL DEVICE (from the platform `.scad`->skin gated assets, CONSUMED at runtime) on
//! `/dev/fb0`, highlights each control the engine prompts for, and drives the headless
//! `pf_input_collect` engine (tsp-bwrg.2) to a candidate `capabilities.toml`.
//!
//! The device skin is READ from the committed, drift-gated platform assets via `--descriptor`
//! (devices/<dev>/capabilities.toml) + `--skin-root` (the dir its `skins/<dev>/*.png` paths resolve
//! against; defaults to the descriptor's grandparent). Ship the CURRENT platform skin alongside the
//! binary (or bake it) so a `.scad` geometry fix propagates on the next deploy — one source of truth.
//!
//! Modes:
//!   pf-collect-ui --dump-dir <dir> --descriptor <caps.toml> [--skin-root <dir>]
//!       Headless: render one PPM per control (no device). For /screen-check render validation.
//!   pf-collect-ui --mode demo --descriptor <caps.toml> [--skin-root <dir>] [--fb /dev/fb0] [--out cand.toml]
//!       On-panel DEMO: synthesize a press per control so the full render + prompt sequence runs
//!       on the panel with NO live pad (proves this bead; real collection is tsp-bwrg.6).
//!   pf-collect-ui --mode live --source /dev/input/eventN --descriptor <caps.toml> ...
//!       On-panel REAL collection against a live evdev node.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use pf_collect_ui::dump;
use pf_collect_ui::fbdev::FbDev;
use pf_collect_ui::skin::SkinSet;
use pf_collect_ui::wizard::{self, Sink, Timing};
use pf_input_collect::collect::DeviceMeta;
use pf_input_collect::source::EvdevSource;

const USAGE: &str = "\
usage:
  pf-collect-ui --dump-dir <dir> --descriptor <caps.toml> [--skin-root <dir>]
      headless: render one PPM per control (no device); for /screen-check validation
  pf-collect-ui --mode demo --descriptor <caps.toml> [--skin-root <dir>] [--fb <path>] [--out <file>]
      on-panel demo: synthesize a press per control (no live pad needed)
  pf-collect-ui --mode live --source <node> --descriptor <caps.toml> [--skin-root <dir>] [--fb <path>] [--out <file>] [--id <id> --manufacturer <m> --model <m>]
      on-panel real collection against a live evdev node

  --descriptor <caps.toml>  the device capabilities.toml (its [skin]/[skin.parts] are consumed)
  --skin-root <dir>         root the descriptor's skins/<dev>/*.png paths resolve against
                            (default: the descriptor's grandparent directory)
  --dump-dir <dir>          headless PPM dump target (implies no device)
  --mode demo|live          default: demo
  --source <node>           live evdev node (required for --mode live)
  --fb <path>               framebuffer device (default: /dev/fb0)
  --id / --manufacturer / --model   identity stamped on the emitted candidate
  --out <file>              write candidate capabilities.toml here (default: stdout)
  -h, --help                this help
";

/// The fbdev-backed frame sink.
struct FbSink(FbDev);
impl Sink for FbSink {
    fn present(&mut self, canvas: &pf_collect_ui::canvas::Canvas) {
        self.0.present(canvas);
    }
}

fn next(args: &mut std::env::Args, flag: &str) -> Option<String> {
    match args.next() {
        Some(v) => Some(v),
        None => {
            eprintln!("error: {flag} needs a value\n\n{USAGE}");
            None
        }
    }
}

fn main() -> ExitCode {
    let mut dump_dir: Option<String> = None;
    let mut mode = String::from("demo");
    let mut source: Option<String> = None;
    let mut fb = String::from("/dev/fb0");
    let mut descriptor: Option<String> = None;
    let mut skin_root: Option<String> = None;
    let mut id: Option<String> = None;
    let mut manufacturer: Option<String> = None;
    let mut model: Option<String> = None;
    let mut out: Option<String> = None;

    let mut args = std::env::args();
    let _argv0 = args.next();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--dump-dir" => match next(&mut args, "--dump-dir") { Some(v) => dump_dir = Some(v), None => return ExitCode::from(2) },
            "--mode" => match next(&mut args, "--mode") { Some(v) => mode = v, None => return ExitCode::from(2) },
            "--source" => match next(&mut args, "--source") { Some(v) => source = Some(v), None => return ExitCode::from(2) },
            "--fb" => match next(&mut args, "--fb") { Some(v) => fb = v, None => return ExitCode::from(2) },
            "--descriptor" => match next(&mut args, "--descriptor") { Some(v) => descriptor = Some(v), None => return ExitCode::from(2) },
            "--skin-root" => match next(&mut args, "--skin-root") { Some(v) => skin_root = Some(v), None => return ExitCode::from(2) },
            "--id" => match next(&mut args, "--id") { Some(v) => id = Some(v), None => return ExitCode::from(2) },
            "--manufacturer" => match next(&mut args, "--manufacturer") { Some(v) => manufacturer = Some(v), None => return ExitCode::from(2) },
            "--model" => match next(&mut args, "--model") { Some(v) => model = Some(v), None => return ExitCode::from(2) },
            "--out" => match next(&mut args, "--out") { Some(v) => out = Some(v), None => return ExitCode::from(2) },
            "-h" | "--help" => { print!("{USAGE}"); return ExitCode::SUCCESS; }
            other => { eprintln!("error: unknown argument '{other}'\n\n{USAGE}"); return ExitCode::from(2); }
        }
    }

    // The device skin is required for every mode — the face IS the consumed device render.
    let descriptor = match descriptor {
        Some(d) => d,
        None => { eprintln!("error: --descriptor <caps.toml> is required (the device skin is consumed from it)\n\n{USAGE}"); return ExitCode::from(2); }
    };
    let descriptor_path = PathBuf::from(&descriptor);
    let skin_root = match skin_root {
        Some(r) => PathBuf::from(r),
        // <root>/devices/<dev>/capabilities.toml -> the skin-root is <root> (three parents up), the
        // dir the descriptor's `skins/<dev>/*.png` paths resolve against.
        None => descriptor_path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".")),
    };
    let skin = match SkinSet::load(&descriptor_path, &skin_root) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: loading device skin from {descriptor} (skin-root {}): {e}", skin_root.display());
            eprintln!("hint: --descriptor must point at the device capabilities.toml and --skin-root at the dir its skins/<dev>/*.png resolve against");
            return ExitCode::FAILURE;
        }
    };

    // Headless dump — no device, no engine drive.
    if let Some(dir) = dump_dir {
        match dump::dump_frames(Path::new(&dir), &skin) {
            Ok(paths) => {
                for p in &paths {
                    println!("{}", p.display());
                }
                eprintln!("pf-collect-ui: wrote {} frames to {dir}", paths.len());
                return ExitCode::SUCCESS;
            }
            Err(e) => { eprintln!("error: dump failed: {e}"); return ExitCode::FAILURE; }
        }
    }

    // On-panel modes need a framebuffer.
    let mut sink = match FbDev::open(&fb) {
        Ok(d) => {
            let f = d.format();
            eprintln!("pf-collect-ui: fb {} {}x{} {}bpp", fb, f.w, f.h, f.bpp);
            FbSink(d)
        }
        Err(e) => {
            eprintln!("error: cannot open framebuffer '{fb}': {e}");
            eprintln!("hint: on-panel modes need /dev/fb0; is another owner (menu/boot-animator) holding it? see docs/RENDER_HOST_DECISION.md");
            return ExitCode::FAILURE;
        }
    };

    let meta = DeviceMeta {
        id: id.unwrap_or_else(|| "demopad".to_string()),
        manufacturer: manufacturer.unwrap_or_else(|| "PocketForge".to_string()),
        model: model.unwrap_or_else(|| "Demo Pad".to_string()),
    };

    let caps = match mode.as_str() {
        "demo" => {
            let mut src = wizard::demo_source();
            wizard::drive_demo(&mut src, &mut sink, &skin, &meta, &Timing::demo())
        }
        "live" => {
            let node = match source {
                Some(s) => s,
                None => { eprintln!("error: --mode live requires --source <node>\n\n{USAGE}"); return ExitCode::from(2); }
            };
            let mut src = match EvdevSource::open(&node) {
                Ok(s) => s,
                Err(e) => { eprintln!("error: cannot open evdev source '{node}': {e}"); return ExitCode::FAILURE; }
            };
            wizard::drive_live(&mut src, &mut sink, &skin, &meta, &Timing::live())
        }
        other => { eprintln!("error: unknown --mode '{other}' (want demo|live)\n\n{USAGE}"); return ExitCode::from(2); }
    };

    match caps {
        Ok(c) => {
            let toml = c.to_toml();
            match out {
                Some(f) => {
                    if let Err(e) = std::fs::write(&f, toml.as_bytes()) {
                        eprintln!("error: writing candidate to {f}: {e}");
                        return ExitCode::FAILURE;
                    }
                    eprintln!("pf-collect-ui: wrote candidate capabilities.toml to {f}");
                }
                None => print!("{toml}"),
            }
            ExitCode::SUCCESS
        }
        Err(e) => { eprintln!("error: collection failed: {e}"); ExitCode::FAILURE }
    }
}
