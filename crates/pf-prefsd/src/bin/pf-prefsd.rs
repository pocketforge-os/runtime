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
    writer_group: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let raw_args: Vec<_> = std::env::args().skip(1).collect();
    if raw_args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{HELP}");
        return Ok(());
    }
    let args = parse_args(raw_args.into_iter())?;
    let writer_gid = resolve_writer_gid(&args.writer_group)?;
    create_dir_all(&args.state_dir, "state directory")?;
    prepare_socket(&args.socket)
        .map_err(|error| path_error("prepare socket", &args.socket, error))?;
    let listener = UnixListener::bind(&args.socket)
        .map_err(|error| path_error("bind socket", &args.socket, error))?;
    let _socket_guard = SocketGuard(args.socket.clone());
    fs::set_permissions(&args.socket, fs::Permissions::from_mode(0o600))
        .map_err(|error| path_error("set socket permissions", &args.socket, error))?;

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
        writer_gid,
        &STOP,
    )?;
    Ok(())
}

fn create_dir_all(path: &Path, description: &str) -> io::Result<()> {
    fs::create_dir_all(path)
        .map_err(|error| path_error(&format!("create {description}"), path, error))
}

fn path_error(operation: &str, path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{operation} {}: {error}", path.display()),
    )
}

fn prepare_socket(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent, "socket parent directory")?;
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
    let mut writer_group = None;
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--state-dir" => state_dir = Some(value.into()),
            "--socket" => socket = Some(value.into()),
            "--writer-group" => writer_group = Some(value),
            _ => return Err(format!("unknown argument: {flag}")),
        }
    }
    Ok(Args {
        state_dir: state_dir.ok_or("--state-dir is required")?,
        socket: socket.ok_or("--socket is required")?,
        writer_group: writer_group.ok_or("--writer-group is required")?,
    })
}

fn resolve_writer_gid(name: &str) -> io::Result<u32> {
    let groups = fs::read_to_string("/etc/group")?;
    group_gid(&groups, name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("writer group '{name}' does not exist"),
        )
    })
}

fn group_gid(groups: &str, name: &str) -> Option<u32> {
    groups.lines().find_map(|line| {
        let mut fields = line.split(':');
        (fields.next()? == name)
            .then(|| fields.nth(1)?.parse().ok())
            .flatten()
    })
}

const HELP: &str = "Usage: pf-prefsd --state-dir PATH --socket PATH --writer-group GROUP";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_args_parse() {
        let args = parse_args(
            [
                "--socket",
                "/tmp/prefs.sock",
                "--state-dir",
                "/tmp/prefs",
                "--writer-group",
                "pf-pref-writer",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(args.socket, PathBuf::from("/tmp/prefs.sock"));
        assert_eq!(args.state_dir, PathBuf::from("/tmp/prefs"));
        assert_eq!(args.writer_group, "pf-pref-writer");
    }

    #[test]
    fn writer_group_lookup_is_exact_and_typed() {
        let groups = "root:x:0:\npf-pref-writer:x:4242:\npf-pref-writer-extra:x:4243:\n";
        assert_eq!(group_gid(groups, "pf-pref-writer"), Some(4242));
        assert_eq!(group_gid(groups, "pf-pref"), None);
    }

    #[test]
    fn installed_unit_requires_the_dedicated_writer_group() {
        let unit = include_str!("../../../../systemd/pf-prefsd.service");
        assert!(unit.contains("--writer-group pf-pref-writer"));
    }

    #[test]
    fn path_errors_name_the_failed_path() {
        let path = Path::new("/run/pocketforge/prefsd.sock");
        let error = path_error(
            "bind socket",
            path,
            io::Error::from(io::ErrorKind::PermissionDenied),
        );
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains(path.to_str().unwrap()));
    }
}
