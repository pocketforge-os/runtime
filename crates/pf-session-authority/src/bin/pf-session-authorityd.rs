use pf_ports::{Clock, MonotonicTime};
use pf_session_authority::{
    run_service_loop, spawn_front_acceptor, spawn_rpc_acceptor, Authority, CommandSystem,
    CommandTemplates, FileStore, FixedUrlHandler, FrontPolicy, PendingRpc,
    DEFAULT_CONNECTION_LIMITS, DEFAULT_PRESENTATION_TIMEOUT, DEFAULT_TICK_INTERVAL,
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

struct Args {
    state_dir: PathBuf,
    socket: PathBuf,
    templates: CommandTemplates,
    /// The private SDK-front socket (tsp-ght0z); absent until the front's unit exists.
    front: Option<FrontArgs>,
    /// The URL handler item id until pf-prefsd's default-handler key lands (tsp-mv7zn).
    url_handler: Option<String>,
}

struct FrontArgs {
    socket: PathBuf,
    user: String,
    unit: String,
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
    let front_policy = match &args.front {
        Some(front) => {
            prepare_socket(&front.socket)?;
            let uid = resolve_uid(&front.user)?;
            Some(FrontPolicy::kernel(uid, front.unit.clone()))
        }
        None => None,
    };
    let mut authority = Authority::open(
        FileStore::new(args.state_dir.join("authority.json")),
        CommandSystem::new(args.templates),
        SystemClock(Instant::now()),
        32,
        Duration::from_secs(10),
    )?
    .with_presentation_timeout(DEFAULT_PRESENTATION_TIMEOUT);
    if let Some(handler) = args.url_handler {
        authority = authority.with_url_handler(FixedUrlHandler(handler));
    }
    authority.reconcile()?;
    // The socket is the daemon's readiness boundary. Publish or reconcile the
    // complete durable state before clients can observe that boundary.
    let listener = UnixListener::bind(&args.socket)?;
    let _socket_guard = SocketGuard(args.socket.clone());
    // Connection threads own all socket I/O (bounded by DEFAULT_CONNECTION_LIMITS) and forward
    // only complete requests, so the single-threaded authority blocks only in the loop's channel
    // wait and its self-driven tick keeps firing during a slow or silent client.
    let (requests, incoming) = mpsc::channel();
    spawn_rpc_acceptor(listener, requests.clone(), DEFAULT_CONNECTION_LIMITS);
    let _front_guard = match (&args.front, front_policy) {
        (Some(front), Some(policy)) => {
            let listener = UnixListener::bind(&front.socket)?;
            spawn_front_acceptor(listener, requests, DEFAULT_CONNECTION_LIMITS, policy);
            Some(SocketGuard(front.socket.clone()))
        }
        _ => None,
    };
    run_service_loop(
        &mut authority,
        &incoming,
        DEFAULT_TICK_INTERVAL,
        |authority, pending: PendingRpc| {
            pending.dispatch(authority);
            Ok(())
        },
    )?;
    Ok(())
}

fn resolve_uid(name: &str) -> io::Result<u32> {
    let passwd = fs::read_to_string("/etc/passwd")?;
    user_uid(&passwd, name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("front user '{name}' does not exist"),
        )
    })
}

fn user_uid(passwd: &str, name: &str) -> Option<u32> {
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        (fields.next()? == name)
            .then(|| fields.nth(1)?.parse().ok())
            .flatten()
    })
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
  --start-command COMMAND          Override the preset's launch command
  --graceful-stop-command COMMAND  Override the preset's graceful-stop command
  --terminate-command COMMAND      Override the preset's forced-termination command
  --activate-owner-command COMMAND Override the preset's selected-owner command
  --front-socket PATH              Private socket for the SDK front (OpenUrl/ReturnToCaller);
                                   requires --front-user and --front-unit
  --front-user NAME                The SDK front's user (socket-bound peer uid)
  --front-unit UNIT                The SDK front's systemd unit (peer cgroup)
  --url-handler ITEM_ID            The pf-app item that opens http(s) URLs
  -h, --help                       Print help"#;

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut state_dir: Option<PathBuf> = None;
    let mut socket: Option<PathBuf> = None;
    let mut preset = "device".to_owned();
    let mut start = None;
    let mut graceful = None;
    let mut terminate = None;
    let mut activate = None;
    let mut front_socket: Option<PathBuf> = None;
    let mut front_user = None;
    let mut front_unit = None;
    let mut url_handler = None;
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--state-dir" => state_dir = Some(value.into()),
            "--socket" => socket = Some(value.into()),
            "--command-preset" => preset = value,
            "--start-command" => start = Some(value),
            "--graceful-stop-command" => graceful = Some(value),
            "--terminate-command" => terminate = Some(value),
            "--activate-owner-command" => activate = Some(value),
            "--front-socket" => front_socket = Some(value.into()),
            "--front-user" => front_user = Some(value),
            "--front-unit" => front_unit = Some(value),
            "--url-handler" => url_handler = Some(value),
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
    let front = match (front_socket, front_user, front_unit) {
        (None, None, None) => None,
        (Some(socket), Some(user), Some(unit)) => Some(FrontArgs { socket, user, unit }),
        _ => return Err("--front-socket, --front-user and --front-unit go together".into()),
    };
    if let Some(handler) = &url_handler {
        pf_app_manifest::validate_app_id(handler)
            .map_err(|_| format!("--url-handler is not an application id: {handler}"))?;
    }
    Ok(Args {
        state_dir,
        socket: socket.ok_or("--socket is required")?,
        templates,
        front,
        url_handler,
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
        assert!(args.front.is_none());
        assert!(args.url_handler.is_none());
    }

    #[test]
    fn front_socket_flags_go_together_and_the_handler_is_an_app_id() {
        let base = ["--state-dir", "/tmp/pf", "--socket", "/tmp/pf.sock"];
        let parse = |extra: &[&str]| parse_args(base.iter().chain(extra).map(|s| (*s).to_owned()));
        let args = parse(&[
            "--front-socket",
            "/run/pocketforge/session-authority-front.sock",
            "--front-user",
            "pf-front",
            "--front-unit",
            "pf-sdk-front.service",
            "--url-handler",
            "org.pocketforge.browser",
        ])
        .unwrap();
        let front = args.front.unwrap();
        assert_eq!(
            front.socket,
            PathBuf::from("/run/pocketforge/session-authority-front.sock")
        );
        assert_eq!(front.user, "pf-front");
        assert_eq!(front.unit, "pf-sdk-front.service");
        assert_eq!(args.url_handler.as_deref(), Some("org.pocketforge.browser"));
        assert!(parse(&["--front-socket", "/tmp/f.sock"]).is_err());
        assert!(parse(&["--url-handler", "not an id"]).is_err());
        assert_eq!(
            user_uid(
                "root:x:0:0::/root:/bin/sh\npf-front:x:991:991::/:/usr/sbin/nologin\n",
                "pf-front"
            ),
            Some(991)
        );
        assert_eq!(user_uid("pf-front-x:x:1:1::/:/bin/sh\n", "pf-front"), None);
    }
}
