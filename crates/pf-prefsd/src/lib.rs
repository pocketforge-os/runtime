//! System preference service protocol and serving loop.

use pf_peer_identity::{
    peer_cgroup_with_source, service_unit_from_cgroup, KernelPeerProcessSource,
    PeerProcessSource,
};
// Peer identity is the shared `pf-peer-identity` crate (one implementation for every service
// that identifies apps); these re-exports keep the daemon's public surface stable.
pub use pf_peer_identity::{peer_cred, verify_peer_uid, PeerCred};
use pf_prefs::{PrefKind, PrefValue, PrefsStore, SCHEMA};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io;
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
    service_unit_from_cgroup(cgroup).and_then(|unit| writer_from_unit(&unit))
}

fn peer_writer_with_source<S: PeerProcessSource>(
    stream: &UnixStream,
    writer_gid: u32,
    source: &S,
) -> io::Result<Option<PreferenceWriter>> {
    let cred = peer_cred(stream)?;
    if !source.peer_groups(stream)?.contains(&writer_gid) {
        return Ok(None);
    }
    let cgroup = peer_cgroup_with_source(stream, cred.pid, source)?;
    Ok(writer_from_cgroup(&cgroup))
}

/// Resolve a writer only when the socket-bound groups contain `writer_gid` and the peer's
/// stable process handle names a trusted systemd cgroup.
pub fn peer_writer(stream: &UnixStream, writer_gid: u32) -> io::Result<Option<PreferenceWriter>> {
    peer_writer_with_source(stream, writer_gid, &KernelPeerProcessSource)
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
///
/// `writer_gid` is the dedicated supplementary group assigned only to control-plane writers.
pub fn serve_until(
    listener: UnixListener,
    store: &PrefsStore,
    allowed_uid: u32,
    writer_gid: u32,
    stop: &AtomicBool,
) -> io::Result<()> {
    serve_until_with_timeout(
        listener,
        store,
        allowed_uid,
        writer_gid,
        stop,
        CONNECTION_TIMEOUT,
    )
}

/// Serve serial connections with an explicit per-I/O timeout.
///
/// The separate entry point lets tests use a short bound without weakening the
/// production deadline.
pub fn serve_until_with_timeout(
    listener: UnixListener,
    store: &PrefsStore,
    allowed_uid: u32,
    writer_gid: u32,
    stop: &AtomicBool,
    connection_timeout: Duration,
) -> io::Result<()> {
    serve_until_with_timeout_and_resolver(
        listener,
        store,
        allowed_uid,
        stop,
        connection_timeout,
        move |stream| peer_writer(stream, writer_gid),
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
use pf_peer_identity::{acquire_peer_process, socket_peer_groups, PeerProcess, PeerProcessKind};
#[cfg(test)]
use std::os::fd::OwnedFd;

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_WRITER_GID: u32 = 42_424;

    #[derive(Clone, Copy)]
    enum ShippingKernel {
        A133Linux49,
        A523Linux515,
    }

    struct ShippingKernelSource {
        kernel: ShippingKernel,
        cgroup: &'static str,
        writer_group: bool,
    }

    #[derive(Clone, Copy)]
    enum FailureStage {
        SocketPidFd,
        PidFdOpen,
    }

    struct UnexpectedErrorSource(FailureStage);

    impl PeerProcessSource for UnexpectedErrorSource {
        fn peer_groups(&self, _stream: &UnixStream) -> io::Result<Vec<u32>> {
            Ok(vec![TEST_WRITER_GID])
        }

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
        fn peer_groups(&self, _stream: &UnixStream) -> io::Result<Vec<u32>> {
            Ok(if self.writer_group {
                vec![TEST_WRITER_GID]
            } else {
                Vec::new()
            })
        }

        fn socket_pidfd(&self, _stream: &UnixStream) -> io::Result<OwnedFd> {
            assert!(
                self.writer_group,
                "an untrusted peer must be denied before numeric PID lookup"
            );
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
                let source = ShippingKernelSource {
                    kernel,
                    cgroup,
                    writer_group: true,
                };
                let writer = peer_writer_with_source(&peer, TEST_WRITER_GID, &source).unwrap();
                assert_eq!(writer, Some(expected));
                for row in WRITE_POLICY {
                    assert!(write_allowed(expected, row.key));
                }
            }

            let source = ShippingKernelSource {
                kernel,
                cgroup: "0::/system.slice/pf-app@settings.service\n",
                writer_group: false,
            };
            assert_eq!(
                peer_writer_with_source(&peer, TEST_WRITER_GID, &source).unwrap(),
                None
            );
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
            let error =
                peer_writer_with_source(&peer, TEST_WRITER_GID, &UnexpectedErrorSource(stage))
                    .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(expected_errno));
        }
    }

    #[test]
    fn socket_peer_groups_match_the_connecting_process() {
        let (peer, _other) = UnixStream::pair().unwrap();
        let mut expected = vec![0u32; 256];
        // SAFETY: `expected` is writable for the supplied element count.
        let count =
            unsafe { libc::getgroups(expected.len() as libc::c_int, expected.as_mut_ptr()) };
        assert!(
            count >= 0,
            "getgroups failed: {}",
            io::Error::last_os_error()
        );
        expected.truncate(count as usize);
        expected.sort_unstable();

        let mut actual = socket_peer_groups(&peer).unwrap();
        actual.sort_unstable();
        assert_eq!(actual, expected);
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

        let source = KernelPeerProcessSource;
        let process = acquire_peer_process(&peer, cred.pid, &source).unwrap();
        assert_eq!(process.kind, PeerProcessKind::SocketPidFd);
        assert_eq!(
            source.cgroup(&process, cred.pid).unwrap(),
            std::fs::read_to_string("/proc/self/cgroup").unwrap()
        );
    }
}
