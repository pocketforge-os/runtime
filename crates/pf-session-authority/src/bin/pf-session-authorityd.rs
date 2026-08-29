use pf_ports::{Clock, MonotonicTime};
use pf_session_authority::{
    serve_connection, Authority, CommandSystem, CommandTemplates, FileStore,
};
use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
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
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args(env::args().skip(1))?;
    fs::create_dir_all(&args.state_dir)?;
    prepare_socket(&args.socket)?;
    let listener = UnixListener::bind(&args.socket)?;
    let _socket_guard = SocketGuard(args.socket.clone());
    let mut authority = Authority::open(
        FileStore::new(args.state_dir.join("authority.json")),
        CommandSystem::new(args.templates),
        SystemClock(Instant::now()),
        32,
        Duration::from_secs(10),
    )?;
    authority.reconcile()?;
    for connection in listener.incoming() {
        let mut stream = connection?;
        let mut writer = stream.try_clone()?;
        if let Err(error) = serve_connection(&mut authority, &mut stream, &mut writer) {
            eprintln!("pf-session-authorityd: connection error: {error:?}");
        }
    }
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

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut state_dir = None;
    let mut socket = None;
    let defaults = CommandTemplates::default();
    let mut start = defaults.start_foreground.join(" ");
    let mut graceful = defaults.request_graceful_stop.join(" ");
    let mut terminate = defaults.enforce_termination.join(" ");
    let mut activate = defaults.activate_selected_owner.join(" ");
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--state-dir" => state_dir = Some(value.into()),
            "--socket" => socket = Some(value.into()),
            "--start-command" => start = value,
            "--graceful-stop-command" => graceful = value,
            "--terminate-command" => terminate = value,
            "--activate-owner-command" => activate = value,
            _ => return Err(format!("unknown argument: {flag}")),
        }
    }
    Ok(Args {
        state_dir: state_dir.ok_or("--state-dir is required")?,
        socket: socket.ok_or("--socket is required")?,
        templates: CommandTemplates::from_strings(&start, &graceful, &terminate, &activate),
    })
}
