//! System preference service protocol and serving loop.

use pf_prefs::{PrefKind, PrefValue, PrefsStore, SCHEMA};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Maximum time a client may block one read or write in the serial v1 server.
pub const CONNECTION_TIMEOUT: Duration = Duration::from_secs(3);

/// One v1 preference request, carried as JSON in a `pf-wire` frame.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum RpcRequest {
    Get { key: String },
    GetAll,
    IsExplicit { key: String },
    Set { key: String, value: Value },
}

/// One v1 preference response.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum RpcResponse {
    Value {
        value: Value,
    },
    Values {
        values: BTreeMap<String, Value>,
    },
    Explicit {
        explicit: bool,
    },
    Ok,
    Error {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<ErrorKind>,
    },
}

/// Machine-readable classification for daemon failures.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    InvalidValue,
    PermissionDenied,
    Store,
    Internal,
    #[serde(other)]
    Unknown,
}

/// Error returned by a short-lived prefs daemon RPC.
#[derive(Debug)]
pub enum ClientError {
    Transport(io::Error),
    Protocol(String),
    Remote {
        message: String,
        kind: Option<ErrorKind>,
    },
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(error) => write!(formatter, "preference daemon unavailable: {error}"),
            Self::Protocol(error) => {
                write!(formatter, "invalid preference daemon response: {error}")
            }
            Self::Remote { message, .. } => {
                write!(formatter, "preference daemon rejected request: {message}")
            }
        }
    }
}

impl std::error::Error for ClientError {}

/// Fresh-connection-per-request client for the serial v1 daemon protocol.
#[derive(Clone, Debug)]
pub struct Client {
    socket: PathBuf,
}

impl Client {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn get(&self, key: &str) -> Result<Value, ClientError> {
        match self.call(&RpcRequest::Get { key: key.into() })? {
            RpcResponse::Value { value } => Ok(value),
            response => Err(unexpected_response(response)),
        }
    }

    pub fn get_all(&self) -> Result<BTreeMap<String, Value>, ClientError> {
        match self.call(&RpcRequest::GetAll)? {
            RpcResponse::Values { values } => Ok(values),
            response => Err(unexpected_response(response)),
        }
    }

    pub fn is_explicit(&self, key: &str) -> Result<bool, ClientError> {
        match self.call(&RpcRequest::IsExplicit { key: key.into() })? {
            RpcResponse::Explicit { explicit } => Ok(explicit),
            response => Err(unexpected_response(response)),
        }
    }

    pub fn set(&self, key: &str, value: Value) -> Result<Value, ClientError> {
        match self.call(&RpcRequest::Set {
            key: key.into(),
            value,
        })? {
            RpcResponse::Value { value } => Ok(value),
            response => Err(unexpected_response(response)),
        }
    }

    fn call(&self, request: &RpcRequest) -> Result<RpcResponse, ClientError> {
        let mut stream = UnixStream::connect(&self.socket).map_err(ClientError::Transport)?;
        stream
            .set_read_timeout(Some(CONNECTION_TIMEOUT))
            .and_then(|()| stream.set_write_timeout(Some(CONNECTION_TIMEOUT)))
            .map_err(ClientError::Transport)?;
        let body = serde_json::to_vec(request)
            .map_err(|error| ClientError::Protocol(error.to_string()))?;
        pf_wire::write_frame(&mut stream, &body)
            .map_err(|error| ClientError::Transport(map_wire_error(error)))?;
        let body = pf_wire::read_frame(&mut stream)
            .map_err(|error| ClientError::Transport(map_wire_error(error)))?;
        match serde_json::from_slice(&body)
            .map_err(|error| ClientError::Protocol(error.to_string()))?
        {
            RpcResponse::Error { message, kind } => Err(ClientError::Remote { message, kind }),
            response => Ok(response),
        }
    }
}

fn unexpected_response(response: RpcResponse) -> ClientError {
    ClientError::Protocol(format!("unexpected response: {response:?}"))
}

/// The peer's kernel-attested Unix credentials.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerCred {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

/// Kernel-derived class of process permitted to change user preferences.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreferenceWriter {
    Settings,
    Shell,
}

/// One explicit row in the default-deny write policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WritePolicyRow {
    pub key: &'static str,
    pub writers: &'static [PreferenceWriter],
}

const SETTINGS_AND_SHELL: &[PreferenceWriter] =
    &[PreferenceWriter::Settings, PreferenceWriter::Shell];

/// Every currently writable preference is named here. Schema additions do not inherit access.
pub const WRITE_POLICY: &[WritePolicyRow] = &[
    WritePolicyRow {
        key: "appearance",
        writers: SETTINGS_AND_SHELL,
    },
    WritePolicyRow {
        key: "textScale",
        writers: SETTINGS_AND_SHELL,
    },
    WritePolicyRow {
        key: "highContrast",
        writers: SETTINGS_AND_SHELL,
    },
    WritePolicyRow {
        key: "reduceFlashing",
        writers: SETTINGS_AND_SHELL,
    },
    WritePolicyRow {
        key: "reduceMotion",
        writers: SETTINGS_AND_SHELL,
    },
    WritePolicyRow {
        key: "hapticsEnabled",
        writers: SETTINGS_AND_SHELL,
    },
    WritePolicyRow {
        key: "monoAudio",
        writers: SETTINGS_AND_SHELL,
    },
    WritePolicyRow {
        key: "brightness",
        writers: SETTINGS_AND_SHELL,
    },
];

/// Return whether a kernel-derived writer class may change `key`.
pub fn write_allowed(writer: PreferenceWriter, key: &str) -> bool {
    WRITE_POLICY
        .iter()
        .find(|row| row.key == key)
        .is_some_and(|row| row.writers.contains(&writer))
}

fn writer_from_unit(unit: &str) -> Option<PreferenceWriter> {
    match unit {
        "pf-settings.service" => Some(PreferenceWriter::Settings),
        "pf-shell-selected.service" => Some(PreferenceWriter::Shell),
        unit if unit
            .strip_prefix("pf-foreground@")
            .and_then(|instance| instance.strip_suffix(".service"))
            .is_some_and(|instance| !instance.is_empty()) =>
        {
            Some(PreferenceWriter::Shell)
        }
        _ => None,
    }
}

/// Classify the systemd service in a `/proc/<pid>/cgroup` document.
pub fn writer_from_cgroup(cgroup: &str) -> Option<PreferenceWriter> {
    cgroup.lines().find_map(|line| {
        let mut fields = line.splitn(3, ':');
        let hierarchy = fields.next()?;
        let controllers = fields.next()?;
        let path = fields.next()?;
        let is_systemd = (hierarchy == "0" && controllers.is_empty())
            || controllers.split(',').any(|name| name == "name=systemd");
        if !is_systemd {
            return None;
        }
        path.rsplit('/')
            .find(|component| component.ends_with(".service"))
            .and_then(writer_from_unit)
    })
}

/// Read `SO_PEERCRED` from an accepted Unix connection.
pub fn peer_cred(stream: &UnixStream) -> io::Result<PeerCred> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the fd is a live Unix socket and `cred` is writable for exactly `len` bytes.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(PeerCred {
        pid: cred.pid,
        uid: cred.uid,
        gid: cred.gid,
    })
}

/// Check a credential against the daemon's uid. Kept separate for direct unit testing.
pub fn verify_peer_uid(cred: PeerCred, allowed_uid: u32) -> io::Result<()> {
    if cred.uid == allowed_uid {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refused peer pid={} uid={} (expected uid={allowed_uid})",
                cred.pid, cred.uid
            ),
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PeerProcessKind {
    SocketPidFd,
    PidFdOpen,
    ProcDir,
}

struct PeerProcess {
    fd: OwnedFd,
    kind: PeerProcessKind,
}

trait PeerProcessSource {
    fn socket_pidfd(&self, stream: &UnixStream) -> io::Result<OwnedFd>;
    fn pidfd_open(&self, pid: i32) -> io::Result<OwnedFd>;
    fn proc_dir_open(&self, pid: i32) -> io::Result<OwnedFd>;
    fn cgroup(&self, process: &PeerProcess, pid: i32) -> io::Result<String>;
}

struct KernelPeerProcessSource;

impl PeerProcessSource for KernelPeerProcessSource {
    fn socket_pidfd(&self, stream: &UnixStream) -> io::Result<OwnedFd> {
        socket_peer_pidfd(stream)
    }

    fn pidfd_open(&self, pid: i32) -> io::Result<OwnedFd> {
        // SAFETY: pidfd_open takes a numeric PID and zero flags, and returns a new owned fd.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) })
    }

    fn proc_dir_open(&self, pid: i32) -> io::Result<OwnedFd> {
        let path = CString::new(format!("/proc/{pid}"))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        // SAFETY: `path` is NUL-terminated and the successful descriptor is owned by the caller.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn cgroup(&self, process: &PeerProcess, pid: i32) -> io::Result<String> {
        match process.kind {
            PeerProcessKind::SocketPidFd | PeerProcessKind::PidFdOpen => {
                verify_live_pidfd(&process.fd, pid)?;
                let cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
                verify_live_pidfd(&process.fd, pid)?;
                Ok(cgroup)
            }
            PeerProcessKind::ProcDir => {
                verify_proc_dir(&process.fd, pid)?;
                let cgroup = read_proc_file_at(&process.fd, c"cgroup")?;
                verify_proc_dir(&process.fd, pid)?;
                Ok(cgroup)
            }
        }
    }
}

fn socket_peer_pidfd(stream: &UnixStream) -> io::Result<OwnedFd> {
    let mut fd: libc::c_int = -1;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: the fd is a live Unix socket and `fd` is writable for exactly `len` bytes.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERPIDFD,
            (&mut fd as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    if fd < 0 || len as usize != std::mem::size_of::<libc::c_int>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SO_PEERPIDFD returned an invalid descriptor",
        ));
    }
    // SAFETY: successful SO_PEERPIDFD returns a new descriptor owned by the caller.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn read_proc_file_at(proc_dir: &OwnedFd, name: &CStr) -> io::Result<String> {
    // SAFETY: `proc_dir` is an open directory and `name` is a NUL-terminated relative name.
    let fd = unsafe {
        libc::openat(
            proc_dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut contents = String::new();
    File::from(unsafe { OwnedFd::from_raw_fd(fd) }).read_to_string(&mut contents)?;
    Ok(contents)
}

fn verify_proc_dir(proc_dir: &OwnedFd, expected_pid: i32) -> io::Result<()> {
    let stat = read_proc_file_at(proc_dir, c"stat")?;
    let pid = stat
        .split_once(' ')
        .map(|(pid, _)| pid)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "process stat has no pid"))?
        .parse::<i32>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if pid == expected_pid {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("proc directory names pid={pid}, expected peer pid={expected_pid}"),
        ))
    }
}

fn socket_pidfd_unsupported(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOPROTOOPT | libc::EINVAL | libc::EOPNOTSUPP)
    )
}

fn acquire_peer_process<S: PeerProcessSource>(
    stream: &UnixStream,
    pid: i32,
    source: &S,
) -> io::Result<PeerProcess> {
    match source.socket_pidfd(stream) {
        Ok(fd) => Ok(PeerProcess {
            fd,
            kind: PeerProcessKind::SocketPidFd,
        }),
        Err(error) if socket_pidfd_unsupported(&error) => match source.pidfd_open(pid) {
            Ok(fd) => Ok(PeerProcess {
                fd,
                kind: PeerProcessKind::PidFdOpen,
            }),
            Err(error) if error.raw_os_error() == Some(libc::ENOSYS) => {
                source.proc_dir_open(pid).map(|fd| PeerProcess {
                    fd,
                    kind: PeerProcessKind::ProcDir,
                })
            }
            Err(error) => Err(error),
        },
        Err(error) => Err(error),
    }
}

fn verify_live_pidfd(pidfd: &OwnedFd, expected_pid: i32) -> io::Result<()> {
    let fdinfo = fs::read_to_string(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd()))?;
    let pid = fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("Pid:\t"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "pidfd has no Pid field"))?
        .parse::<i32>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if pid != expected_pid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("pidfd names pid={pid}, expected peer pid={expected_pid}"),
        ));
    }
    let mut pollfd = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `pollfd` points to one initialized descriptor and the zero timeout never blocks.
    let rc = unsafe { libc::poll(&mut pollfd, 1, 0) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    if rc != 0 || pollfd.revents != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "peer exited during identity lookup",
        ));
    }
    Ok(())
}

fn peer_writer_with_source<S: PeerProcessSource>(
    stream: &UnixStream,
    source: &S,
) -> io::Result<Option<PreferenceWriter>> {
    let cred = peer_cred(stream)?;
    let process = acquire_peer_process(stream, cred.pid, source)?;
    let cgroup = source.cgroup(&process, cred.pid)?;
    Ok(writer_from_cgroup(&cgroup))
}

/// Resolve the write class from the live socket peer's stable process handle and systemd cgroup.
pub fn peer_writer(stream: &UnixStream) -> io::Result<Option<PreferenceWriter>> {
    peer_writer_with_source(stream, &KernelPeerProcessSource)
}

/// Serve exactly one request and response on a connection.
pub fn serve_connection(store: &PrefsStore, stream: &mut UnixStream) -> io::Result<()> {
    serve_connection_as(store, stream, None)
}

/// Serve one request using a writer identity established outside the request payload.
pub fn serve_connection_as(
    store: &PrefsStore,
    stream: &mut UnixStream,
    writer: Option<PreferenceWriter>,
) -> io::Result<()> {
    let body = pf_wire::read_frame(stream).map_err(map_wire_error)?;
    let response = match serde_json::from_slice::<RpcRequest>(&body) {
        Ok(request) => handle_rpc(store, request, writer),
        Err(error) => RpcResponse::Error {
            message: format!("invalid request: {error}"),
            kind: Some(ErrorKind::Internal),
        },
    };
    let body = serde_json::to_vec(&response).map_err(io::Error::other)?;
    pf_wire::write_frame(stream, &body).map_err(map_wire_error)
}

fn map_wire_error(error: pf_wire::WireError) -> io::Error {
    match error {
        // Preserve timeout kinds so an incomplete length prefix/body or a blocked
        // response is explicitly handled as a connection I/O failure.
        pf_wire::WireError::Io(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            error
        }
        error => io::Error::other(error),
    }
}

/// Serve serial, short-lived connections until `stop` is set.
pub fn serve_until(
    listener: UnixListener,
    store: &PrefsStore,
    allowed_uid: u32,
    stop: &AtomicBool,
) -> io::Result<()> {
    serve_until_with_timeout(listener, store, allowed_uid, stop, CONNECTION_TIMEOUT)
}

/// Serve serial connections with an explicit per-I/O timeout.
///
/// The separate entry point lets tests use a short bound without weakening the
/// production deadline.
pub fn serve_until_with_timeout(
    listener: UnixListener,
    store: &PrefsStore,
    allowed_uid: u32,
    stop: &AtomicBool,
    connection_timeout: Duration,
) -> io::Result<()> {
    serve_until_with_timeout_and_resolver(
        listener,
        store,
        allowed_uid,
        stop,
        connection_timeout,
        peer_writer,
    )
}

/// Serving loop with an injected kernel-identity resolver for hermetic policy tests.
pub fn serve_until_with_timeout_and_resolver<F>(
    listener: UnixListener,
    store: &PrefsStore,
    allowed_uid: u32,
    stop: &AtomicBool,
    connection_timeout: Duration,
    resolve_writer: F,
) -> io::Result<()>
where
    F: Fn(&UnixStream) -> io::Result<Option<PreferenceWriter>>,
{
    listener.set_nonblocking(true)?;
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let cred = match peer_cred(&stream) {
                    Ok(cred) => cred,
                    Err(error) => {
                        eprintln!("pf-prefsd: peer refused: {error}");
                        continue;
                    }
                };
                if let Err(error) = verify_peer_uid(cred, allowed_uid) {
                    eprintln!("pf-prefsd: peer refused: {error}");
                    continue;
                }
                let writer = match resolve_writer(&stream) {
                    Ok(writer) => writer,
                    Err(error) => {
                        eprintln!(
                            "pf-prefsd: peer pid={} has no write identity: {error}",
                            cred.pid
                        );
                        None
                    }
                };
                if let Err(error) = stream
                    .set_nonblocking(false)
                    .and_then(|()| stream.set_read_timeout(Some(connection_timeout)))
                    .and_then(|()| stream.set_write_timeout(Some(connection_timeout)))
                {
                    eprintln!("pf-prefsd: connection setup error: {error}");
                    continue;
                }
                if let Err(error) = serve_connection_as(store, &mut stream, writer) {
                    eprintln!("pf-prefsd: connection error: {error}");
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn handle_rpc(
    store: &PrefsStore,
    request: RpcRequest,
    writer: Option<PreferenceWriter>,
) -> RpcResponse {
    if let RpcRequest::Set { key, .. } = &request {
        if !writer.is_some_and(|writer| write_allowed(writer, key)) {
            return RpcResponse::Error {
                message: format!("writer is not authorized to change preference '{key}'"),
                kind: Some(ErrorKind::PermissionDenied),
            };
        }
    }
    let result: Result<RpcResponse, (ErrorKind, pf_prefs::PrefError)> = match request {
        RpcRequest::Get { key } => store
            .load()
            .and_then(|prefs| prefs.value(&key))
            .map(|value| RpcResponse::Value {
                value: value_to_json(value),
            })
            .map_err(|error| (classify_pref_error(&error), error)),
        RpcRequest::GetAll => store
            .load()
            .and_then(|prefs| {
                SCHEMA
                    .iter()
                    .map(|spec| {
                        prefs
                            .value(spec.key)
                            .map(|value| (spec.key.to_owned(), value_to_json(value)))
                    })
                    .collect()
            })
            .map(|values| RpcResponse::Values { values })
            .map_err(|error| (classify_pref_error(&error), error)),
        RpcRequest::Set { key, value } => json_to_value(&key, value)
            .map_err(|error| (ErrorKind::InvalidValue, error))
            .and_then(|value| {
                store
                    .apply(&key, value)
                    .map_err(|error| (classify_pref_error(&error), error))
            })
            .and_then(|_| {
                store
                    .load()
                    .and_then(|prefs| prefs.value(&key))
                    .map_err(|error| (classify_pref_error(&error), error))
            })
            .map(|value| RpcResponse::Value {
                value: value_to_json(value),
            }),
        RpcRequest::IsExplicit { key } => store
            .load()
            .map(|prefs| RpcResponse::Explicit {
                explicit: prefs.is_explicit(&key),
            })
            .map_err(|error| (classify_pref_error(&error), error)),
    };
    result.unwrap_or_else(|(kind, error)| RpcResponse::Error {
        message: error.to_string(),
        kind: Some(kind),
    })
}

fn classify_pref_error(error: &pf_prefs::PrefError) -> ErrorKind {
    match error {
        pf_prefs::PrefError::UnknownKey(_)
        | pf_prefs::PrefError::Type { .. }
        | pf_prefs::PrefError::Range { .. } => ErrorKind::InvalidValue,
        pf_prefs::PrefError::Io(_)
        | pf_prefs::PrefError::Parse(_)
        | pf_prefs::PrefError::UnsupportedVersion { .. } => ErrorKind::Store,
    }
}

fn json_to_value(key: &str, value: Value) -> Result<PrefValue, pf_prefs::PrefError> {
    let spec =
        pf_prefs::spec(key).ok_or_else(|| pf_prefs::PrefError::UnknownKey(key.to_owned()))?;
    let candidate = match spec.kind {
        PrefKind::Bool => value.as_bool().map(PrefValue::Bool),
        PrefKind::Scalar { .. } => value.as_i64().map(PrefValue::Scalar),
        PrefKind::Enum { variants } => value
            .as_str()
            .and_then(|raw| variants.iter().copied().find(|variant| *variant == raw))
            .map(PrefValue::Enum),
    }
    .ok_or_else(|| pf_prefs::PrefError::Type {
        key: key.to_owned(),
        expected: pf_prefs::schema::kind_name(spec.kind),
        got: json_kind(&value),
    })?;
    pf_prefs::validate(key, candidate)
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Null => "null",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn value_to_json(value: PrefValue) -> Value {
    match value {
        PrefValue::Bool(value) => Value::Bool(value),
        PrefValue::Scalar(value) => Value::Number(value.into()),
        PrefValue::Enum(value) => Value::String(value.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    enum ShippingKernel {
        A133Linux49,
        A523Linux515,
    }

    struct ShippingKernelSource {
        kernel: ShippingKernel,
        cgroup: &'static str,
    }

    #[derive(Clone, Copy)]
    enum FailureStage {
        SocketPidFd,
        PidFdOpen,
    }

    struct UnexpectedErrorSource(FailureStage);

    impl PeerProcessSource for UnexpectedErrorSource {
        fn socket_pidfd(&self, _stream: &UnixStream) -> io::Result<OwnedFd> {
            match self.0 {
                FailureStage::SocketPidFd => Err(io::Error::from_raw_os_error(libc::EACCES)),
                FailureStage::PidFdOpen => Err(io::Error::from_raw_os_error(libc::ENOPROTOOPT)),
            }
        }

        fn pidfd_open(&self, _pid: i32) -> io::Result<OwnedFd> {
            Err(io::Error::from_raw_os_error(libc::EPERM))
        }

        fn proc_dir_open(&self, _pid: i32) -> io::Result<OwnedFd> {
            panic!("unexpected identity error must not select the proc fallback")
        }

        fn cgroup(&self, _process: &PeerProcess, _pid: i32) -> io::Result<String> {
            panic!("an unverified process handle must not be classified")
        }
    }

    impl PeerProcessSource for ShippingKernelSource {
        fn socket_pidfd(&self, _stream: &UnixStream) -> io::Result<OwnedFd> {
            Err(io::Error::from_raw_os_error(libc::ENOPROTOOPT))
        }

        fn pidfd_open(&self, _pid: i32) -> io::Result<OwnedFd> {
            match self.kernel {
                ShippingKernel::A133Linux49 => Err(io::Error::from_raw_os_error(libc::ENOSYS)),
                ShippingKernel::A523Linux515 => Ok(std::fs::File::open("/dev/null")?.into()),
            }
        }

        fn proc_dir_open(&self, _pid: i32) -> io::Result<OwnedFd> {
            Ok(std::fs::File::open("/dev/null")?.into())
        }

        fn cgroup(&self, process: &PeerProcess, _pid: i32) -> io::Result<String> {
            let expected = match self.kernel {
                ShippingKernel::A133Linux49 => PeerProcessKind::ProcDir,
                ShippingKernel::A523Linux515 => PeerProcessKind::PidFdOpen,
            };
            assert_eq!(process.kind, expected);
            Ok(self.cgroup.to_owned())
        }
    }

    #[test]
    fn shipping_kernel_fallbacks_keep_trusted_writers_and_apps_denied() {
        let (peer, _other) = UnixStream::pair().unwrap();
        for kernel in [ShippingKernel::A133Linux49, ShippingKernel::A523Linux515] {
            for (cgroup, expected) in [
                (
                    "0::/system.slice/pf-settings.service\n",
                    PreferenceWriter::Settings,
                ),
                (
                    "0::/user.slice/user-1000.slice/pf-shell-selected.service\n",
                    PreferenceWriter::Shell,
                ),
                (
                    "0::/system.slice/pf-foreground@main.service\n",
                    PreferenceWriter::Shell,
                ),
            ] {
                let source = ShippingKernelSource { kernel, cgroup };
                let writer = peer_writer_with_source(&peer, &source).unwrap();
                assert_eq!(writer, Some(expected));
                for row in WRITE_POLICY {
                    assert!(write_allowed(expected, row.key));
                }
            }

            let source = ShippingKernelSource {
                kernel,
                cgroup: "0::/system.slice/pf-app@settings.service\n",
            };
            assert_eq!(peer_writer_with_source(&peer, &source).unwrap(), None);
        }
    }

    #[test]
    fn production_fallback_handles_pin_and_read_the_current_process() {
        let source = KernelPeerProcessSource;
        let pid = std::process::id() as i32;
        let expected = std::fs::read_to_string("/proc/self/cgroup").unwrap();

        let pidfd = PeerProcess {
            fd: source.pidfd_open(pid).unwrap(),
            kind: PeerProcessKind::PidFdOpen,
        };
        assert_eq!(source.cgroup(&pidfd, pid).unwrap(), expected);

        let proc_dir = PeerProcess {
            fd: source.proc_dir_open(pid).unwrap(),
            kind: PeerProcessKind::ProcDir,
        };
        assert_eq!(source.cgroup(&proc_dir, pid).unwrap(), expected);
    }

    #[test]
    fn compatibility_fallbacks_do_not_swallow_unexpected_identity_errors() {
        let (peer, _other) = UnixStream::pair().unwrap();
        for (stage, expected_errno) in [
            (FailureStage::SocketPidFd, libc::EACCES),
            (FailureStage::PidFdOpen, libc::EPERM),
        ] {
            let error = peer_writer_with_source(&peer, &UnexpectedErrorSource(stage)).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(expected_errno));
        }
    }

    #[test]
    fn peer_uid_verification_accepts_match_and_rejects_mismatch() {
        let cred = PeerCred {
            pid: 7,
            uid: 42,
            gid: 9,
        };
        assert!(verify_peer_uid(cred, 42).is_ok());
        assert_eq!(
            verify_peer_uid(cred, 41).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn app_cgroup_set_is_refused_while_settings_and_shell_are_accepted() {
        let root =
            std::env::temp_dir().join(format!("pf-prefsd-write-policy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = PrefsStore::at(&root);
        let request = || RpcRequest::Set {
            key: "reduceMotion".into(),
            value: Value::Bool(true),
        };

        let app = writer_from_cgroup("0::/system.slice/pf-app@steamlink.service\n");
        assert_eq!(app, None);
        assert!(matches!(
            handle_rpc(&store, request(), app),
            RpcResponse::Error {
                kind: Some(ErrorKind::PermissionDenied),
                ..
            }
        ));
        assert!(!root.join("prefs.json").exists(), "denial must not mutate");

        for (cgroup, expected_writer) in [
            (
                "0::/system.slice/pf-settings.service\n",
                PreferenceWriter::Settings,
            ),
            (
                "0::/user.slice/user-1000.slice/pf-shell-selected.service\n",
                PreferenceWriter::Shell,
            ),
            (
                "0::/system.slice/pf-foreground@main.service\n",
                PreferenceWriter::Shell,
            ),
        ] {
            let writer = writer_from_cgroup(cgroup);
            assert_eq!(writer, Some(expected_writer));
            assert!(matches!(
                handle_rpc(&store, request(), writer),
                RpcResponse::Value {
                    value: Value::Bool(true)
                }
            ));
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn write_policy_covers_schema_exactly_and_unknown_keys_default_deny() {
        let policy_keys: std::collections::BTreeSet<_> =
            WRITE_POLICY.iter().map(|row| row.key).collect();
        let schema_keys: std::collections::BTreeSet<_> = SCHEMA.iter().map(|row| row.key).collect();
        assert_eq!(policy_keys, schema_keys);
        assert_eq!(
            WRITE_POLICY.len(),
            schema_keys.len(),
            "duplicate policy row"
        );

        for spec in SCHEMA {
            assert!(write_allowed(PreferenceWriter::Settings, spec.key));
            assert!(write_allowed(PreferenceWriter::Shell, spec.key));
        }
        assert!(!write_allowed(PreferenceWriter::Settings, "futureKey"));
    }

    #[test]
    fn cgroup_identity_ignores_app_broker_scopes_and_client_like_text() {
        for cgroup in [
            "0::/system.slice/pf-app@settings.service\n",
            "0::/system.slice/pf-input-broker.service\n",
            "0::/user.slice/user-1000.slice/session-3.scope\n",
            "0::/system.slice/not-pf-settings.service\n",
            "5:freezer:/pf-settings.service\n",
        ] {
            assert_eq!(writer_from_cgroup(cgroup), None, "{cgroup:?}");
        }
        assert_eq!(
            writer_from_cgroup("1:name=systemd:/system.slice/pf-settings.service\n"),
            Some(PreferenceWriter::Settings)
        );
    }

    #[test]
    fn socket_peer_pidfd_stabilizes_the_proc_cgroup_lookup() {
        let (peer, _other) = UnixStream::pair().unwrap();
        let cred = peer_cred(&peer).unwrap();
        assert_eq!(cred.pid, std::process::id() as i32);
        assert_eq!(cred.uid, unsafe { libc::geteuid() });

        let expected = writer_from_cgroup(&std::fs::read_to_string("/proc/self/cgroup").unwrap());
        assert_eq!(peer_writer(&peer).unwrap(), expected);
    }
}
