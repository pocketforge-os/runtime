use pf_ports::{Clock, MonotonicTime};
use pf_session_authority::orientation::{
    refresh_from_source, spawn_input_driver, OrientationInputs,
};
use pf_session_authority::orientation_source::{
    production_descriptor_path, LinuxOrientationPaths, LinuxOrientationSource,
};
use pf_session_authority::{
    run_service_loop, spawn_front_acceptor, spawn_rpc_acceptor_with_orientation, Authority,
    CommandSystem, CommandTemplates, FileStore, FixedUrlHandler, FrontPolicy, PendingRpc,
    SessionOwnerConfig, ShellPolicy, DEFAULT_CONNECTION_LIMITS, DEFAULT_GRACE_PERIOD,
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
    shell_user: String,
    shell_unit: String,
    broker_unit: String,
    compositor_unit: String,
    home_client_unit: Option<String>,
    home_client_presentation: Option<PathBuf>,
    descriptor: Option<PathBuf>,
    prefs_socket: PathBuf,
    orientation_fixture_root: Option<PathBuf>,
    orientation_live: bool,
    templates: CommandTemplates,
    /// The private SDK-front socket (tsp-ght0z); absent until the front's unit exists.
    front: Option<FrontArgs>,
    /// The URL handler item id until pf-prefsd's default-handler key lands (tsp-mv7zn).
    url_handler: Option<String>,
    grace: Duration,
}

#[derive(Debug)]
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
    let shell_uid = resolve_uid(&args.shell_user)?;
    let shell_policy = ShellPolicy::kernel_with_compositor(
        shell_uid,
        args.shell_unit.clone(),
        args.broker_unit.clone(),
        args.compositor_unit.clone(),
    )?;
    let mut authority = Authority::open(
        FileStore::new(args.state_dir.join("authority.json")),
        CommandSystem::new_with_session_owner(
            args.templates,
            match (args.home_client_unit, args.home_client_presentation) {
                (Some(unit), Some(signal)) => SessionOwnerConfig::new(unit, signal)?,
                (None, None) => SessionOwnerConfig::default(),
                _ => unreachable!("paired home client arguments are validated by parse_args"),
            },
        ),
        SystemClock(Instant::now()),
        32,
        args.grace,
    )?
    .with_presentation_timeout(DEFAULT_PRESENTATION_TIMEOUT);
    if let Some(handler) = args.url_handler {
        authority = authority.with_url_handler(FixedUrlHandler(handler));
    }
    authority.reconcile()?;
    let orientation_source = if args.orientation_live {
        let mut orientation_paths = LinuxOrientationPaths {
            descriptor: args
                .descriptor
                .map(Ok)
                .unwrap_or_else(production_descriptor_path)?,
            prefs_socket: args.prefs_socket,
            ..LinuxOrientationPaths::default()
        };
        if let Some(root) = args.orientation_fixture_root {
            orientation_paths.app_root = root.join("apps");
            orientation_paths.drm_class = root.join("drm");
            orientation_paths.device_tree = root.join("dt");
            orientation_paths.input_class = root.join("input-class");
            orientation_paths.input_devices = root.join("input-devices");
            orientation_paths.iio_devices = root.join("iio");
            orientation_paths.sensor_orientation_file = Some(root.join("sensor-orientation"));
            orientation_paths.switch_state_file = Some(root.join("switches.json"));
        }
        Some(LinuxOrientationSource::open(orientation_paths)?)
    } else {
        None
    };
    let _orientation_driver = if let Some(mut orientation_source) = orientation_source {
        let orientation_controller = authority.orientation_controller();
        let orientation_active_app = authority.orientation_active_app();
        let mut orientation_inputs = OrientationInputs::default();
        refresh_from_source(
            &mut orientation_source,
            &orientation_controller,
            &orientation_active_app,
            &mut orientation_inputs,
        )?;
        Some(spawn_input_driver(
            orientation_source,
            orientation_controller,
            orientation_active_app,
            orientation_inputs,
            DEFAULT_TICK_INTERVAL,
        ))
    } else {
        None
    };
    // The socket is the daemon readiness boundary. The first complete snapshot gates it; panel
    // metadata cannot block that snapshot because the source supplies a typed degraded fallback.
    let listener = UnixListener::bind(&args.socket)?;
    let _socket_guard = SocketGuard(args.socket.clone());
    // Connection threads own all socket I/O (bounded by DEFAULT_CONNECTION_LIMITS) and forward
    // only complete requests, so the single-threaded authority blocks only in the loop's channel
    // wait and its self-driven tick keeps firing during a slow or silent client.
    let (requests, incoming) = mpsc::channel();
    let orientation = authority.orientation_watch();
    spawn_rpc_acceptor_with_orientation(
        listener,
        requests.clone(),
        DEFAULT_CONNECTION_LIMITS,
        shell_policy,
        orientation,
    );
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
            format!("configured user '{name}' does not exist"),
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
  --grace-period-ms MS             Graceful-stop window before self-escalation (default 1500;
                                   keep it plus the hang sampler's budget inside the app
                                   unit's TimeoutStopSec)
  --start-command COMMAND          Override the preset's launch command
  --graceful-stop-command COMMAND  Override the preset's graceful-stop command
  --terminate-command COMMAND      Override the preset's forced-termination command
  --activate-owner-command COMMAND Override the preset's selected-owner command
  --shell-user NAME                Shell socket user (default gamer)
  --shell-unit UNIT                Full-access shell unit (default pf-shell-selected.service)
  --broker-unit UNIT               Root SafeReturn-only unit (default pf-input-broker.service)
  --compositor-unit UNIT           Read-only orientation consumer (default gamescope-session.service)
  --home-client-unit UNIT          Gamescope session home client (optional; paired with signal)
  --home-client-presentation PATH  Compositor-owned focused-home signal (paired with unit)
  --descriptor PATH               Device capabilities descriptor (default $PF_DESCRIPTOR,
                                   $PF_PLATFORM/devices/$PF_DEVICE_ID/capabilities.toml,
                                   or the sole installed per-device descriptor)
  --prefsd-socket PATH             User rotation preferences (default /run/pocketforge/prefsd.sock)
  --orientation-mode MODE          live (default); static only with desktop-sim test preset
  --orientation-fixture-root PATH  Hermetic Linux-input tree; desktop-sim tests only
  --front-socket PATH              Private socket for the SDK front (OpenUrl/ReturnToCaller);
                                   requires --front-user and --front-unit
  --front-user NAME                The SDK front's user (socket-bound peer uid)
  --front-unit UNIT                The SDK front's systemd unit (peer cgroup)
  --url-handler ITEM_ID            The pf-app item that opens http(s) URLs
  -h, --help                       Print help"#;

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut state_dir: Option<PathBuf> = None;
    let mut socket: Option<PathBuf> = None;
    let mut shell_user = "gamer".to_owned();
    let mut shell_unit = "pf-shell-selected.service".to_owned();
    let mut broker_unit = "pf-input-broker.service".to_owned();
    let mut compositor_unit = "gamescope-session.service".to_owned();
    let mut home_client_unit = None;
    let mut home_client_presentation: Option<PathBuf> = None;
    let defaults = LinuxOrientationPaths::default();
    let mut descriptor = None;
    let mut prefs_socket = defaults.prefs_socket;
    let mut orientation_fixture_root = None;
    let mut orientation_live = true;
    let mut preset = "device".to_owned();
    let mut grace = DEFAULT_GRACE_PERIOD;
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
            "--shell-user" => shell_user = value,
            "--shell-unit" => shell_unit = value,
            "--broker-unit" => broker_unit = value,
            "--compositor-unit" => compositor_unit = value,
            "--home-client-unit" => home_client_unit = Some(value),
            "--home-client-presentation" => home_client_presentation = Some(value.into()),
            "--descriptor" => descriptor = Some(value.into()),
            "--prefsd-socket" => prefs_socket = value.into(),
            "--orientation-fixture-root" => orientation_fixture_root = Some(value.into()),
            "--orientation-mode" => match value.as_str() {
                "live" => orientation_live = true,
                "static" => orientation_live = false,
                _ => return Err(format!("unknown orientation mode: {value}")),
            },
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
    if !orientation_live && preset != "desktop-sim" {
        return Err("static orientation mode requires --command-preset desktop-sim".into());
    }
    if orientation_fixture_root.is_some() && preset != "desktop-sim" {
        return Err("--orientation-fixture-root requires --command-preset desktop-sim".into());
    }
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
    match (&home_client_unit, &home_client_presentation) {
        (None, None) => {}
        (Some(unit), Some(signal)) => {
            SessionOwnerConfig::new(unit.clone(), signal.clone())?;
        }
        _ => {
            return Err("--home-client-unit and --home-client-presentation go together".to_owned())
        }
    }
    Ok(Args {
        state_dir,
        socket: socket.ok_or("--socket is required")?,
        shell_user,
        shell_unit,
        broker_unit,
        compositor_unit,
        home_client_unit,
        home_client_presentation,
        descriptor,
        prefs_socket,
        orientation_fixture_root,
        orientation_live,
        templates,
        front,
        url_handler,
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
        assert!(args.front.is_none());
        assert!(args.url_handler.is_none());
        assert_eq!(args.shell_user, "gamer");
        assert_eq!(args.shell_unit, "pf-shell-selected.service");
        assert_eq!(args.broker_unit, "pf-input-broker.service");
        assert!(args.orientation_live);
    }

    #[test]
    fn static_orientation_is_confined_to_the_desktop_sim_preset() {
        let base = ["--state-dir", "/tmp/pf", "--socket", "/tmp/pf.sock"];
        let parse = |extra: &[&str]| parse_args(base.iter().chain(extra).map(|s| (*s).to_owned()));
        assert!(parse(&["--orientation-mode", "static"]).is_err());
        assert!(
            !parse(&[
                "--command-preset",
                "desktop-sim",
                "--orientation-mode",
                "static",
            ])
            .unwrap()
            .orientation_live
        );
        assert!(parse(&["--orientation-fixture-root", "/tmp/fixture"]).is_err());
        assert_eq!(
            parse(&[
                "--command-preset",
                "desktop-sim",
                "--orientation-fixture-root",
                "/tmp/fixture",
            ])
            .unwrap()
            .orientation_fixture_root,
            Some(PathBuf::from("/tmp/fixture"))
        );
    }

    #[test]
    fn front_socket_flags_go_together_and_the_handler_is_an_app_id() {
        let base = ["--state-dir", "/tmp/pf", "--socket", "/tmp/pf.sock"];
        let parse = |extra: &[&str]| parse_args(base.iter().chain(extra).map(|s| (*s).to_owned()));
        let args = parse(&[
            "--shell-user",
            "player",
            "--shell-unit",
            "custom-shell.service",
            "--broker-unit",
            "custom-broker.service",
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
        assert_eq!(args.shell_user, "player");
        assert_eq!(args.shell_unit, "custom-shell.service");
        assert_eq!(args.broker_unit, "custom-broker.service");
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

    #[test]
    fn home_client_configuration_is_explicit_and_paired() {
        let base = ["--state-dir", "/tmp/pf", "--socket", "/tmp/pf.sock"];
        let parse = |extra: &[&str]| parse_args(base.iter().chain(extra).map(|s| (*s).to_owned()));
        let args = parse(&[
            "--home-client-unit",
            "pf-shell-gamescope.service",
            "--home-client-presentation",
            "/run/pocketforge/session/focused-home-unit",
        ])
        .unwrap();
        assert_eq!(
            args.home_client_unit.as_deref(),
            Some("pf-shell-gamescope.service")
        );
        assert_eq!(
            args.home_client_presentation.as_deref(),
            Some(Path::new("/run/pocketforge/session/focused-home-unit"))
        );
        assert!(parse(&["--home-client-unit", "pf-shell-gamescope.service"]).is_err());
        assert!(parse(&[
            "--home-client-presentation",
            "/run/pocketforge/session/focused-home-unit"
        ])
        .is_err());
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
