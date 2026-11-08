//! `pf-app-fetch --app-id <id> --descriptor <app.toml> --state-dir <dir> [--ca-file <pem>]`
//!
//! Stages an app's pinned `[fetch]` block into `<state-dir>` (infra-107.4 §4.6, W3 of
//! tsp-ziac.4.3). Run by `pf-app-fetch@<id>.service` as the app's state owner (gamer), never
//! root. Exit 0 = Ready, 1 = failed with `reason=<code>` on stderr and in
//! `<state-dir>/.pf-fetch/result.json` (what the launcher, W4, reads), 2 = usage.

mod codes;
mod confine;
mod descriptor;
mod extract;
mod hooks;
mod http;
mod scope;
mod txn;

use codes::Code;
use serde_json::json;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

static CANCEL: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop(_signal: libc::c_int) {
    CANCEL.store(true, Ordering::SeqCst);
}

fn install_stop_handler() {
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: a zeroed sigaction with a valid async-signal-safe handler (one atomic
        // store). No SA_RESTART, so a blocking read returns EINTR and the run can cancel.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_stop as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

const USAGE: &str =
    "usage: pf-app-fetch --app-id <id> --descriptor <app.toml> --state-dir <dir> [--ca-file <pem>]";

fn valid_app_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 255
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

fn truncate(detail: &str, max: usize) -> String {
    let mut end = detail.len().min(max);
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    detail[..end].to_string()
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (mut app_id, mut descriptor, mut state_dir) = (None, None, None);
    let mut ca_file = PathBuf::from("/etc/ssl/certs/ca-certificates.crt");
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--version" => {
                println!(
                    "pf-app-fetch {} test-hooks={}",
                    env!("CARGO_PKG_VERSION"),
                    if hooks::ENABLED { "on" } else { "off" }
                );
                return ExitCode::SUCCESS;
            }
            "--app-id" => app_id = args.next(),
            "--descriptor" => descriptor = args.next().map(PathBuf::from),
            "--state-dir" => state_dir = args.next().map(PathBuf::from),
            "--ca-file" => match args.next() {
                Some(path) => ca_file = PathBuf::from(path),
                None => {
                    eprintln!("{USAGE}");
                    return ExitCode::from(2);
                }
            },
            _ => {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let (Some(app_id), Some(descriptor), Some(state_dir)) = (app_id, descriptor, state_dir) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    if !valid_app_id(&app_id) || !state_dir.is_absolute() {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    install_stop_handler();
    let confine = if hooks::ENABLED {
        match hooks::var("CONFINE").as_deref() {
            Some("required") => confine::Mode::Required,
            Some("best-effort") => confine::Mode::BestEffort,
            _ => confine::Mode::Off,
        }
    } else {
        confine::Mode::Required
    };
    let config = txn::Config {
        app_id: app_id.clone(),
        descriptor,
        state_dir: state_dir.clone(),
        ca_file,
        confine,
    };
    let mut report = txn::Report {
        check: "not_run",
        gets: 0,
        landlock: None,
    };
    let outcome = txn::run(&config, &CANCEL, &mut report);
    let run_id = txn::random_hex();
    let (result, exit) = match &outcome {
        Ok(()) => {
            eprintln!(
                "pf-app-fetch: app={app_id} outcome=ready check={} gets={} landlock={}",
                report.check,
                report.gets,
                report
                    .landlock
                    .map_or("off".to_string(), |abi| format!("abi{abi}"))
            );
            (
                json!({
                    "schema_version": 1,
                    "run_id": run_id,
                    "app_id": app_id,
                    "outcome": "ready",
                    "reason": null,
                    "class": null,
                    "retryable": null,
                    "detail": "",
                    "check": report.check,
                    "gets": report.gets,
                }),
                ExitCode::SUCCESS,
            )
        }
        Err(failure) => {
            let code: Code = failure.code;
            let detail = truncate(&failure.detail, 200);
            eprintln!(
                "pf-app-fetch: app={app_id} outcome=failed reason={} class={} retryable={} check={} gets={} detail={detail}",
                code.as_str(),
                code.class(),
                code.retryable(),
                report.check,
                report.gets
            );
            (
                json!({
                    "schema_version": 1,
                    "run_id": run_id,
                    "app_id": app_id,
                    "outcome": "failed",
                    "reason": code.as_str(),
                    "class": code.class(),
                    "retryable": code.retryable(),
                    "detail": detail,
                    "check": report.check,
                    "gets": report.gets,
                }),
                ExitCode::from(1),
            )
        }
    };
    let fetch_dir = state_dir.join(".pf-fetch");
    if state_dir.is_dir() && !fetch_dir.exists() {
        use std::os::unix::fs::DirBuilderExt;
        let _ = std::fs::DirBuilder::new().mode(0o755).create(&fetch_dir);
    }
    if fetch_dir.is_dir() {
        let mut text = serde_json::to_vec(&result).expect("result serializes");
        text.push(b'\n');
        if let Err(e) = txn::write_atomic(&fetch_dir, "result.json", &text) {
            eprintln!("pf-app-fetch: could not write result.json: {e}");
        }
    }
    exit
}
