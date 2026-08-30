use pf_prefs::PrefsStore;
use pf_prefsd::serve_until;
use std::fs;
use std::io;
use std::os::raw::c_int;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_signal: c_int) {
    STOP.store(true, Ordering::Relaxed);
}

struct Args {
    state_dir: PathBuf,
    socket: PathBuf,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let raw_args: Vec<_> = std::env::args().skip(1).collect();
    if raw_args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{HELP}");
        return Ok(());
    }
    let args = parse_args(raw_args.into_iter())?;
    fs::create_dir_all(&args.state_dir)?;
    prepare_socket(&args.socket)?;
    let listener = UnixListener::bind(&args.socket)?;
    let _socket_guard = SocketGuard(args.socket.clone());
    fs::set_permissions(&args.socket, fs::Permissions::from_mode(0o600))?;

    // SAFETY: handlers perform only an atomic store, which is async-signal-safe.
    let handler = on_signal as extern "C" fn(c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }

    let allowed_uid = unsafe { libc::geteuid() };
    serve_until(
        listener,
        &PrefsStore::at(args.state_dir),
        allowed_uid,
        &STOP,
    )?;
    Ok(())
}

fn prepare_socket(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            if UnixStream::connect(path).is_ok() {
                Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "another preference daemon is already listening",
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
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--state-dir" => state_dir = Some(value.into()),
            "--socket" => socket = Some(value.into()),
            _ => return Err(format!("unknown argument: {flag}")),
        }
    }
    Ok(Args {
        state_dir: state_dir.ok_or("--state-dir is required")?,
        socket: socket.ok_or("--socket is required")?,
    })
}

const HELP: &str = "Usage: pf-prefsd --state-dir PATH --socket PATH";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_args_parse() {
        let args = parse_args(
            ["--socket", "/tmp/prefs.sock", "--state-dir", "/tmp/prefs"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(args.socket, PathBuf::from("/tmp/prefs.sock"));
        assert_eq!(args.state_dir, PathBuf::from("/tmp/prefs"));
    }
}
