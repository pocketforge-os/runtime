//! `pf-collect-ui` — the on-panel guided-collection wizard (tsp-bwrg.3).
//!
//! Renders a canonical gamepad face on `/dev/fb0`, highlights each control the engine prompts for,
//! and drives the headless `pf_input_collect` engine (tsp-bwrg.2) to a candidate `capabilities.toml`.
//!
//! Modes:
//!   pf-collect-ui --dump-dir <dir>
//!       Headless: render one PPM per control (no device). For /screen-check render validation.
//!   pf-collect-ui --mode demo [--fb /dev/fb0] [--out cand.toml]
//!       On-panel DEMO: synthesize a press per control so the full render + prompt sequence runs
//!       on the panel with NO live pad (proves this bead on-panel; real collection is tsp-bwrg.6).
//!   pf-collect-ui --mode live --source /dev/input/eventN --id <id> --manufacturer <m> --model <m> \
//!                 [--fb /dev/fb0] [--out cand.toml]
//!       On-panel REAL collection against a live evdev node.

use std::process::ExitCode;

use pf_collect_ui::dump;
use pf_collect_ui::fbdev::FbDev;
use pf_collect_ui::wizard::{self, Sink, Timing};
use pf_input_collect::collect::DeviceMeta;
use pf_input_collect::source::EvdevSource;

const USAGE: &str = "\
usage:
  pf-collect-ui --dump-dir <dir>
      headless: render one PPM per control (no device); for /screen-check validation
  pf-collect-ui --mode demo [--fb <path>] [--out <file>] [--id <id>] [--manufacturer <m>] [--model <m>]
      on-panel demo: synthesize a press per control (no live pad needed)
  pf-collect-ui --mode live --source <node> --id <id> --manufacturer <m> --model <m> [--fb <path>] [--out <file>]
      on-panel real collection against a live evdev node

  --dump-dir <dir>      headless PPM dump target (implies no device)
  --mode demo|live      default: demo
  --source <node>       live evdev node (required for --mode live)
  --fb <path>           framebuffer device (default: /dev/fb0)
  --id / --manufacturer / --model   identity stamped on the emitted candidate
  --out <file>          write candidate capabilities.toml here (default: stdout)
  -h, --help            this help
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
            "--id" => match next(&mut args, "--id") { Some(v) => id = Some(v), None => return ExitCode::from(2) },
            "--manufacturer" => match next(&mut args, "--manufacturer") { Some(v) => manufacturer = Some(v), None => return ExitCode::from(2) },
            "--model" => match next(&mut args, "--model") { Some(v) => model = Some(v), None => return ExitCode::from(2) },
            "--out" => match next(&mut args, "--out") { Some(v) => out = Some(v), None => return ExitCode::from(2) },
            "-h" | "--help" => { print!("{USAGE}"); return ExitCode::SUCCESS; }
            other => { eprintln!("error: unknown argument '{other}'\n\n{USAGE}"); return ExitCode::from(2); }
        }
    }

    // Headless dump — no device, no engine drive.
    if let Some(dir) = dump_dir {
        match dump::dump_frames(std::path::Path::new(&dir)) {
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
            wizard::drive_demo(&mut src, &mut sink, &meta, &Timing::demo())
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
            wizard::drive_live(&mut src, &mut sink, &meta, &Timing::live())
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
