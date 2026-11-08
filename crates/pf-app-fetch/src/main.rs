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
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

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
    let config = Arc::new(txn::Config {
        run_id: txn::random_hex(),
        app_id,
        descriptor,
        state_dir,
        ca_file,
        confine,
    });
    let report = Arc::new(Mutex::new(txn::Report {
        check: "not_run",
        ..txn::Report::default()
    }));
    let fetch_dir = config.state_dir.join(".pf-fetch");

    let outcome = match txn::prepare(&config, &report) {
        Err(failure) => Err(failure),
        Ok(prepared) => {
            // Steps 1-7 run on a worker (it inherits the Landlock domain). The main thread
            // polls the stop flag, so SIGTERM is answered within ~100 ms even while a read
            // is stalled inside ureq/rustls (which retry EINTR internally).
            let (tx, rx) = mpsc::channel();
            {
                let report = report.clone();
                thread::spawn(move || {
                    let _ = tx.send(txn::execute(prepared, &CANCEL, &report));
                });
            }
            loop {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(outcome) => break outcome,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        break Err(codes::Failure::new(Code::StateInvalid, "worker exited"))
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if CANCEL.load(Ordering::SeqCst) {
                            let gate = txn::COMMIT_GATE.lock().unwrap_or_else(|e| e.into_inner());
                            if *gate {
                                // The commit is under way or done: let it finish.
                                drop(gate);
                                break rx.recv().unwrap_or_else(|_| {
                                    Err(codes::Failure::new(Code::StateInvalid, "worker exited"))
                                });
                            }
                            // Holding the gate, the worker cannot enter the commit. Remove
                            // staging, report, and exit; the process exit stops the worker.
                            txn::cleanup_after_cancel(&fetch_dir);
                            let failure = codes::Failure::new(Code::Cancelled, "stop requested");
                            finish(&config, &report, &fetch_dir, Err(failure));
                            std::process::exit(1);
                        }
                    }
                }
            }
        }
    };
    finish(&config, &report, &fetch_dir, outcome)
}

/// Log the outcome and, only for a run that holds the lock, write result.json.
fn finish(
    config: &txn::Config,
    report: &Mutex<txn::Report>,
    fetch_dir: &std::path::Path,
    outcome: codes::Result<()>,
) -> ExitCode {
    let report = report.lock().unwrap_or_else(|e| e.into_inner());
    let landlock = report
        .landlock
        .map_or("off".to_string(), |abi| format!("abi{abi}"));
    let app_id = &config.app_id;
    let exit = match &outcome {
        Ok(()) => {
            eprintln!(
                "pf-app-fetch: app={app_id} run={} outcome=ready check={} gets={} landlock={landlock}",
                config.run_id, report.check, report.gets
            );
            ExitCode::SUCCESS
        }
        Err(failure) => {
            let code: Code = failure.code;
            eprintln!(
                "pf-app-fetch: app={app_id} run={} outcome=failed reason={} class={} retryable={} check={} gets={} landlock={landlock} detail={}",
                config.run_id,
                code.as_str(),
                code.class(),
                code.retryable(),
                report.check,
                report.gets,
                txn::truncate(&failure.detail, 200)
            );
            ExitCode::from(1)
        }
    };
    if report.locked {
        let value = txn::result_value(config, &report, outcome.as_ref().err(), false);
        if let Err(e) = txn::write_result(fetch_dir, &value) {
            eprintln!("pf-app-fetch: could not write result.json: {e}");
        }
    } else {
        eprintln!("pf-app-fetch: app={app_id} did not hold the lock; result.json left untouched");
    }
    exit
}
