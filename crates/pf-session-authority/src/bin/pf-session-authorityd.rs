use pf_ports::{Clock, MonotonicTime};
use pf_session_authority::{
    dispatch_rpc, run_service_loop, spawn_rpc_acceptor, Authority, CommandSystem, CommandTemplates,
    FileStore, PendingRpc, DEFAULT_CONNECTION_LIMITS, DEFAULT_GRACE_PERIOD,
    DEFAULT_PRESENTATION_TIMEOUT, DEFAULT_TICK_INTERVAL,
};
use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct SystemClock(Instant);
impl Clock for SystemClock {
    fn now(&self) -> MonotonicTime {
        MonotonicTime::from_nanos(self.0.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64)
    }
}

#[derive(Debug)]
struct Args {
    state_dir: PathBuf,
    socket: PathBuf,
    templates: CommandTemplates,
    grace: Duration,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let raw_args: Vec<_> = env::args().skip(1).collect();
    if raw_args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{HELP}");
        return Ok(());
    }
    let args = parse_args(raw_args.into_iter())?;
    fs::create_dir_all(&args.state_dir)?;
    prepare_socket(&args.socket)?;
    let mut authority = Authority::open(
        FileStore::new(args.state_dir.join("authority.json")),
        CommandSystem::new(args.templates),
        SystemClock(Instant::now()),
        32,
        args.grace,
    )?
    .with_presentation_timeout(DEFAULT_PRESENTATION_TIMEOUT);
    authority.reconcile()?;
    // The socket is the daemon's readiness boundary. Publish or reconcile the
    // complete durable state before clients can observe that boundary.
    let listener = UnixListener::bind(&args.socket)?;
    let _socket_guard = SocketGuard(args.socket.clone());
    // Connection threads own all socket I/O (bounded by DEFAULT_CONNECTION_LIMITS) and forward
    // only complete requests, so the single-threaded authority blocks only in the loop's channel
    // wait and its self-driven tick keeps firing during a slow or silent client.
    let (requests, incoming) = mpsc::channel();
    spawn_rpc_acceptor(listener, requests, DEFAULT_CONNECTION_LIMITS);
    run_service_loop(
        &mut authority,
        &incoming,
        DEFAULT_TICK_INTERVAL,
        |authority, pending: PendingRpc| {
            let response = dispatch_rpc(authority, pending.request.clone());
            pending.respond(response);
            Ok(())
        },
    )?;
    Ok(())
}

fn prepare_socket(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "another authority is already listening",
                ))
            } else {
                fs::remove_file(path)
            }
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "socket path exists and is not a socket",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

const HELP: &str = r#"Usage: pf-session-authorityd --state-dir PATH --socket PATH [OPTIONS]

Options:
  --command-preset PRESET          Command templates to use: device (default), desktop-sim
                                   desktop-sim maintains sessions/{session_id}.running and
                                   shell-selected markers below --state-dir
  --grace-period-ms MS             Graceful-stop window before self-escalation (default 1500;
                                   keep it plus the hang sampler's budget inside the app
                                   unit's TimeoutStopSec)
  --start-command COMMAND          Override the preset's launch command
  --graceful-stop-command COMMAND  Override the preset's graceful-stop command
  --terminate-command COMMAND      Override the preset's forced-termination command
  --activate-owner-command COMMAND Override the preset's selected-owner command
  -h, --help                       Print help"#;

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut state_dir: Option<PathBuf> = None;
    let mut socket: Option<PathBuf> = None;
    let mut preset = "device".to_owned();
    let mut grace = DEFAULT_GRACE_PERIOD;
    let mut start = None;
    let mut graceful = None;
    let mut terminate = None;
    let mut activate = None;
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--state-dir" => state_dir = Some(value.into()),
            "--socket" => socket = Some(value.into()),
            "--command-preset" => preset = value,
            "--grace-period-ms" => {
                grace =
                    Duration::from_millis(value.parse::<u64>().map_err(|error| {
                        format!("--grace-period-ms expects milliseconds: {error}")
                    })?)
            }
            "--start-command" => start = Some(value),
            "--graceful-stop-command" => graceful = Some(value),
            "--terminate-command" => terminate = Some(value),
            "--activate-owner-command" => activate = Some(value),
            _ => return Err(format!("unknown argument: {flag}")),
        }
    }
    let state_dir = state_dir.ok_or("--state-dir is required")?;
    let mut templates = match preset.as_str() {
        "device" => CommandTemplates::default(),
        "desktop-sim" => CommandTemplates::desktop_sim(&state_dir),
        _ => return Err(format!("unknown command preset: {preset}")),
    };
    if let Some(command) = start {
        templates.start_foreground = command.split_whitespace().map(str::to_owned).collect();
    }
    if let Some(command) = graceful {
        templates.request_graceful_stop = command.split_whitespace().map(str::to_owned).collect();
    }
    if let Some(command) = terminate {
        templates.enforce_termination = command.split_whitespace().map(str::to_owned).collect();
    }
    if let Some(command) = activate {
        templates.activate_selected_owner = command.split_whitespace().map(str::to_owned).collect();
    }
    Ok(Args {
        state_dir,
        socket: socket.ok_or("--socket is required")?,
        templates,
        grace,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_sim_preset_parses_and_explicit_commands_override_it() {
        let args = parse_args(
            [
                "--start-command",
                "custom {session_id}",
                "--command-preset",
                "desktop-sim",
                "--state-dir",
                "/tmp/pf state",
                "--socket",
                "/tmp/pf.sock",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap();

        assert_eq!(args.templates.start_foreground, ["custom", "{session_id}"]);
        assert_eq!(args.templates.request_graceful_stop[0], "sh");
        assert_eq!(args.templates.request_graceful_stop[4], "/tmp/pf state");
    }

    #[test]
    fn grace_period_defaults_to_the_backstop_budget_and_a_flag_overrides_it() {
        let args = parse_args(
            ["--state-dir", "/tmp/pf", "--socket", "/tmp/pf.sock"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(args.grace, DEFAULT_GRACE_PERIOD);
        // Budget: grace (1500 ms) + root sampler (≤300 ms) stays inside the app unit's own
        // TimeoutStopSec=2 s, so the authority escalates before the systemd backstop fires.
        assert_eq!(DEFAULT_GRACE_PERIOD, Duration::from_millis(1500));

        let args = parse_args(
            [
                "--state-dir",
                "/tmp/pf",
                "--socket",
                "/tmp/pf.sock",
                "--grace-period-ms",
                "2500",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(args.grace, Duration::from_millis(2500));

        let error = parse_args(
            [
                "--state-dir",
                "/tmp/pf",
                "--socket",
                "/tmp/pf.sock",
                "--grace-period-ms",
                "soon",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap_err();
        assert!(error.contains("--grace-period-ms"), "{error}");
    }
}
