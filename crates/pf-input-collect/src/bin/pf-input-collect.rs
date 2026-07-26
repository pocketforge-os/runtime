//! `pf-input-collect` — the headless guided-collection CLI (`tsp-bwrg.2`).
//!
//! Walks the default gamepad plan against a raw evdev node, prompting for each control, and writes
//! a candidate `capabilities.toml`. This is the CLI companion to the programmatic engine the
//! on-panel UI (`tsp-bwrg.3`) drives; both run the SAME `pf_input_collect::collect::run` logic.
//!
//! Usage:
//!   pf-input-collect --source /dev/input/event3 --id <deviceid> \
//!                    --manufacturer "<mfr>" --model "<model>" [--out candidate.toml]
//!
//! Validate the result on a host with Python 3.11+:
//!   cp candidate.toml platform/devices/<id>/capabilities.toml && pf caps validate <id>
//!   (or: platform/core/caps.py validate <id>)

use std::process::ExitCode;

use pf_input_collect::{
    collect::{self, DeviceMeta, RunConfig},
    plan, source::EvdevSource, Collector,
};

fn usage() -> &'static str {
    "usage: pf-input-collect --source <node> --id <id> --manufacturer <mfr> --model <model> [--out <file>]\n\
     \n\
     --source <node>        raw evdev node to read (e.g. /dev/input/event3)\n\
     --id <id>              identity.id for the new device (lowercase alnum, e.g. a133)\n\
     --manufacturer <mfr>   identity.manufacturer\n\
     --model <model>        identity.model\n\
     --out <file>           write candidate TOML here (default: stdout)\n"
}

fn main() -> ExitCode {
    let mut source: Option<String> = None;
    let mut id: Option<String> = None;
    let mut manufacturer: Option<String> = None;
    let mut model: Option<String> = None;
    let mut out: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let val = |args: &mut std::iter::Skip<std::env::Args>, flag: &str| -> Option<String> {
            match args.next() {
                Some(v) => Some(v),
                None => {
                    eprintln!("error: {flag} needs a value\n\n{}", usage());
                    None
                }
            }
        };
        match a.as_str() {
            "--source" => match val(&mut args, "--source") { Some(v) => source = Some(v), None => return ExitCode::from(2) },
            "--id" => match val(&mut args, "--id") { Some(v) => id = Some(v), None => return ExitCode::from(2) },
            "--manufacturer" => match val(&mut args, "--manufacturer") { Some(v) => manufacturer = Some(v), None => return ExitCode::from(2) },
            "--model" => match val(&mut args, "--model") { Some(v) => model = Some(v), None => return ExitCode::from(2) },
            "--out" => match val(&mut args, "--out") { Some(v) => out = Some(v), None => return ExitCode::from(2) },
            "-h" | "--help" => { print!("{}", usage()); return ExitCode::SUCCESS; }
            other => { eprintln!("error: unknown argument '{other}'\n\n{}", usage()); return ExitCode::from(2); }
        }
    }

    let (source, id, manufacturer, model) = match (source, id, manufacturer, model) {
        (Some(s), Some(i), Some(m), Some(md)) => (s, i, m, md),
        _ => { eprintln!("error: --source, --id, --manufacturer, --model are all required\n\n{}", usage()); return ExitCode::from(2); }
    };

    let mut src = match EvdevSource::open(&source) {
        Ok(s) => s,
        Err(e) => { eprintln!("error: cannot open evdev source '{source}': {e}"); return ExitCode::FAILURE; }
    };

    let meta = DeviceMeta { id, manufacturer, model };
    let mut collector = Collector::new(plan::default_gamepad_plan());
    let mut stderr = std::io::stderr();

    // Prompts go to stderr so stdout carries only the emitted TOML when --out is unset.
    let caps = match collect::run(&mut collector, &mut src, &meta, &RunConfig::default(), &mut stderr) {
        Ok(c) => c,
        Err(e) => { eprintln!("error: collection failed: {e}"); return ExitCode::FAILURE; }
    };

    let toml = caps.to_toml();
    match out {
        Some(path) => {
            if let Err(e) = std::fs::write(&path, toml) {
                eprintln!("error: cannot write '{path}': {e}");
                return ExitCode::FAILURE;
            }
            eprintln!("wrote candidate: {path}");
        }
        None => print!("{toml}"),
    }
    ExitCode::SUCCESS
}
