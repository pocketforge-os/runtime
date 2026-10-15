use pf_hang_sampler::server::Sampler;
use pf_hang_sampler::transport;
use std::collections::BTreeSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

const SD_LISTEN_FDS_START: libc::c_int = 3;

#[derive(Debug, PartialEq, Eq)]
enum ListenerMode {
    Systemd,
    Path(PathBuf),
}

#[derive(Debug)]
enum SocketActivationError {
    MissingEnvironment { name: &'static str },
    InvalidEnvironment { name: &'static str },
    ListenPidMismatch { expected: u32, actual: u32 },
    ListenFdsMismatch { actual: u32 },
    InheritedFdUnavailable { errno: i32 },
    CloseOnExecFailed { errno: i32 },
    SocketOptionFailed { option: &'static str, errno: i32 },
    SocketDomainMismatch { actual: libc::c_int },
    SocketTypeMismatch { actual: libc::c_int },
    NotListening,
}

impl fmt::Display for SocketActivationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEnvironment { name } => write!(formatter, "missing {name}"),
            Self::InvalidEnvironment { name } => write!(formatter, "invalid {name}"),
            Self::ListenPidMismatch { expected, actual } => {
                write!(
                    formatter,
                    "LISTEN_PID mismatch: expected {expected}, got {actual}"
                )
            }
            Self::ListenFdsMismatch { actual } => {
                write!(formatter, "LISTEN_FDS must be exactly 1, got {actual}")
            }
            Self::InheritedFdUnavailable { errno } => {
                write!(formatter, "inherited fd 3 is unavailable (errno={errno})")
            }
            Self::CloseOnExecFailed { errno } => {
                write!(
                    formatter,
                    "could not mark inherited fd close-on-exec (errno={errno})"
                )
            }
            Self::SocketOptionFailed { option, errno } => {
                write!(
                    formatter,
                    "could not verify inherited socket {option} (errno={errno})"
                )
            }
            Self::SocketDomainMismatch { actual } => {
                write!(formatter, "inherited fd is not AF_UNIX (domain={actual})")
            }
            Self::SocketTypeMismatch { actual } => {
                write!(
                    formatter,
                    "inherited fd is not SOCK_SEQPACKET (type={actual})"
                )
            }
            Self::NotListening => write!(formatter, "inherited socket is not listening"),
        }
    }
}

impl std::error::Error for SocketActivationError {}

fn take_listen_environment() -> (Option<OsString>, Option<OsString>) {
    let mut listen_pid = None;
    let mut listen_fds = None;
    for (name, value) in env::vars_os() {
        if !name.as_bytes().starts_with(b"LISTEN_") {
            continue;
        }
        if name == OsStr::new("LISTEN_PID") {
            listen_pid = Some(value);
        } else if name == OsStr::new("LISTEN_FDS") {
            listen_fds = Some(value);
        }
        env::remove_var(name);
    }
    (listen_pid, listen_fds)
}

fn parse_activation_number(
    value: Option<OsString>,
    name: &'static str,
) -> Result<u32, SocketActivationError> {
    let value = value.ok_or(SocketActivationError::MissingEnvironment { name })?;
    value
        .into_string()
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or(SocketActivationError::InvalidEnvironment { name })
}

fn socket_option(
    fd: libc::c_int,
    option: libc::c_int,
    name: &'static str,
) -> Result<libc::c_int, SocketActivationError> {
    let mut value = 0;
    let mut length = std::mem::size_of_val(&value) as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            (&mut value as *mut libc::c_int).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(SocketActivationError::SocketOptionFailed {
            option: name,
            errno: io::Error::last_os_error().raw_os_error().unwrap_or(0),
        });
    }
    Ok(value)
}

fn adopt_systemd_listener() -> Result<OwnedFd, SocketActivationError> {
    let (listen_pid, listen_fds) = take_listen_environment();
    let actual_pid = parse_activation_number(listen_pid, "LISTEN_PID")?;
    // SAFETY: getpid has no preconditions and cannot fail for a live process.
    let expected_pid = unsafe { libc::getpid() as u32 };
    if actual_pid != expected_pid {
        return Err(SocketActivationError::ListenPidMismatch {
            expected: expected_pid,
            actual: actual_pid,
        });
    }
    let fd_count = parse_activation_number(listen_fds, "LISTEN_FDS")?;
    if fd_count != 1 {
        return Err(SocketActivationError::ListenFdsMismatch { actual: fd_count });
    }

    let flags = unsafe { libc::fcntl(SD_LISTEN_FDS_START, libc::F_GETFD) };
    if flags < 0 {
        return Err(SocketActivationError::InheritedFdUnavailable {
            errno: io::Error::last_os_error().raw_os_error().unwrap_or(0),
        });
    }
    let listener = unsafe { OwnedFd::from_raw_fd(SD_LISTEN_FDS_START) };
    if unsafe {
        libc::fcntl(
            listener.as_raw_fd(),
            libc::F_SETFD,
            flags | libc::FD_CLOEXEC,
        )
    } < 0
    {
        return Err(SocketActivationError::CloseOnExecFailed {
            errno: io::Error::last_os_error().raw_os_error().unwrap_or(0),
        });
    }

    let domain = socket_option(listener.as_raw_fd(), libc::SO_DOMAIN, "SO_DOMAIN")?;
    if domain != libc::AF_UNIX {
        return Err(SocketActivationError::SocketDomainMismatch { actual: domain });
    }
    let socket_type = socket_option(listener.as_raw_fd(), libc::SO_TYPE, "SO_TYPE")?;
    if socket_type != libc::SOCK_SEQPACKET {
        return Err(SocketActivationError::SocketTypeMismatch {
            actual: socket_type,
        });
    }
    let accepting = socket_option(listener.as_raw_fd(), libc::SO_ACCEPTCONN, "SO_ACCEPTCONN")?;
    if accepting != 1 {
        return Err(SocketActivationError::NotListening);
    }
    Ok(listener)
}

fn decode_hex(text: &str) -> Result<Vec<u8>, String> {
    if !(16..=64).contains(&text.len())
        || text.len() % 2 != 0
        || !text.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("build ids must be 8..32 bytes of hexadecimal".to_owned());
    }
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(|| "invalid build id".to_owned())
        })
        .collect()
}

fn prepare_socket(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            if transport::connect(path).is_ok() {
                Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "another sampler is already listening",
                ))
            } else {
                fs::remove_file(path)
            }
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "sampler socket path exists and is not a socket",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn parse_args_from(
    mut args: impl Iterator<Item = String>,
) -> Result<(ListenerMode, BTreeSet<Vec<u8>>), String> {
    let mut listener = None;
    let mut build_ids = BTreeSet::new();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--systemd-socket" => {
                if listener.replace(ListenerMode::Systemd).is_some() {
                    return Err("listener mode specified more than once".to_owned());
                }
            }
            "--socket" => {
                let value = args.next().ok_or("missing value for --socket")?;
                if listener
                    .replace(ListenerMode::Path(PathBuf::from(value)))
                    .is_some()
                {
                    return Err("listener mode specified more than once".to_owned());
                }
            }
            "--allowed-build-id" => {
                let value = args.next().ok_or("missing value for --allowed-build-id")?;
                build_ids.insert(decode_hex(&value)?);
            }
            _ => return Err(format!("unknown argument: {flag}")),
        }
    }
    Ok((listener.ok_or("a listener mode is required")?, build_ids))
}

fn parse_args() -> Result<(ListenerMode, BTreeSet<Vec<u8>>), String> {
    parse_args_from(env::args().skip(1))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (mode, build_ids) = parse_args()?;
    let listener = match mode {
        ListenerMode::Systemd => adopt_systemd_listener()?,
        ListenerMode::Path(socket) => {
            prepare_socket(&socket)?;
            transport::bind(&socket)?
        }
    };
    let mut sampler = Sampler::new(build_ids);
    loop {
        let stream = transport::accept(listener.as_fd())?;
        let (outcome, _completed) = sampler.handle_stream(&stream);
        // The completed safe frame set stays in process until pf-crashd consumes this crate's
        // canonical report API. Never print request data, paths, registers, stack words, or frames.
        eprintln!("{}", outcome.line());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd, IntoRawFd};
    use std::os::unix::net::UnixListener;
    use std::process::Command;

    #[test]
    fn build_id_allowlist_is_exact_hex() {
        assert_eq!(decode_hex("0011223344556677").unwrap().len(), 8);
        assert!(decode_hex("0011").is_err());
        assert!(decode_hex("00112233445566zz").is_err());
    }

    #[test]
    fn listener_mode_is_explicit_and_exclusive() {
        let (mode, _) = parse_args_from(["--systemd-socket".to_owned()].into_iter()).unwrap();
        assert_eq!(mode, ListenerMode::Systemd);

        let (mode, _) =
            parse_args_from(["--socket".to_owned(), "/tmp/test.sock".to_owned()].into_iter())
                .unwrap();
        assert_eq!(mode, ListenerMode::Path(PathBuf::from("/tmp/test.sock")));

        assert!(parse_args_from(std::iter::empty()).is_err());
        assert!(parse_args_from(
            [
                "--systemd-socket".to_owned(),
                "--socket".to_owned(),
                "/tmp/test.sock".to_owned(),
            ]
            .into_iter(),
        )
        .is_err());
    }

    fn run_activation_case(case: &str) {
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::systemd_activation_case",
                "--ignored",
                "--nocapture",
            ])
            .env("PF_SYSTEMD_ACTIVATION_CASE", case)
            .status()
            .unwrap();
        assert!(status.success(), "activation helper failed for {case}");
    }

    fn install_fd_three(raw: libc::c_int) {
        if raw == 3 {
            return;
        }
        assert_eq!(
            unsafe { libc::dup2(raw, 3) },
            3,
            "dup2 failed: {}",
            io::Error::last_os_error()
        );
        assert_eq!(
            unsafe { libc::close(raw) },
            0,
            "close failed: {}",
            io::Error::last_os_error()
        );
    }

    fn remove_socket_if_present(path: &Path) {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => panic!("socket cleanup failed: {error}"),
        }
    }

    fn filesystem_listener(socket_type: libc::c_int) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "pf-hang-sampler-activation-{}-{socket_type}.sock",
            std::process::id()
        ));
        remove_socket_if_present(&path);
        match socket_type {
            libc::SOCK_SEQPACKET => {
                let listener = transport::bind(&path).unwrap();
                install_fd_three(listener.into_raw_fd());
            }
            libc::SOCK_STREAM => {
                let listener = UnixListener::bind(&path).unwrap();
                install_fd_three(listener.into_raw_fd());
            }
            _ => unreachable!(),
        }
        path
    }

    fn non_listening_seqpacket() {
        let raw =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        assert!(raw >= 0, "socket failed: {}", io::Error::last_os_error());
        install_fd_three(raw);
    }

    #[test]
    #[ignore = "subprocess helper"]
    fn systemd_activation_case() {
        let case = std::env::var("PF_SYSTEMD_ACTIVATION_CASE").unwrap();
        let socket_path = match case.as_str() {
            "stream" => Some(filesystem_listener(libc::SOCK_STREAM)),
            "not-listening" => {
                non_listening_seqpacket();
                None
            }
            _ => Some(filesystem_listener(libc::SOCK_SEQPACKET)),
        };

        let pid = std::process::id();
        std::env::set_var(
            "LISTEN_PID",
            if case == "wrong-pid" { pid + 1 } else { pid }.to_string(),
        );
        std::env::set_var("LISTEN_FDS", if case == "two-fds" { "2" } else { "1" });
        std::env::set_var("LISTEN_FDNAMES", "hang-sampler");
        std::env::set_var("LISTEN_TEST_CANARY", "must-be-unset");

        let result = adopt_systemd_listener();
        for name in [
            "LISTEN_PID",
            "LISTEN_FDS",
            "LISTEN_FDNAMES",
            "LISTEN_TEST_CANARY",
        ] {
            assert!(std::env::var_os(name).is_none(), "{name} was not unset");
        }
        match case.as_str() {
            "valid" => {
                let listener = result.unwrap();
                assert_eq!(listener.as_raw_fd(), 3);
                let flags = unsafe { libc::fcntl(listener.as_raw_fd(), libc::F_GETFD) };
                assert_ne!(flags & libc::FD_CLOEXEC, 0);
            }
            "wrong-pid" => assert!(matches!(
                result,
                Err(SocketActivationError::ListenPidMismatch { .. })
            )),
            "two-fds" => assert!(matches!(
                result,
                Err(SocketActivationError::ListenFdsMismatch { actual: 2 })
            )),
            "stream" => assert!(matches!(
                result,
                Err(SocketActivationError::SocketTypeMismatch { .. })
            )),
            "not-listening" => assert!(matches!(result, Err(SocketActivationError::NotListening))),
            _ => panic!("unknown activation case"),
        }

        if let Some(path) = socket_path {
            fs::remove_file(path).expect("socket cleanup failed");
        }
    }

    #[test]
    fn valid_inherited_listener_is_accepted() {
        run_activation_case("valid");
    }

    #[test]
    fn wrong_listen_pid_is_refused() {
        run_activation_case("wrong-pid");
    }

    #[test]
    fn two_inherited_fds_are_refused() {
        run_activation_case("two-fds");
    }

    #[test]
    fn stream_socket_is_refused() {
        run_activation_case("stream");
    }

    #[test]
    fn non_listening_socket_is_refused() {
        run_activation_case("not-listening");
    }
}
