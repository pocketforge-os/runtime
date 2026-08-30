//! System preference service protocol and serving loop.

use pf_prefs::{PrefKind, PrefValue, PrefsStore, SCHEMA};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io;
use std::os::fd::AsRawFd;
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

/// Serve exactly one request and response on a connection.
pub fn serve_connection(store: &PrefsStore, stream: &mut UnixStream) -> io::Result<()> {
    let body = pf_wire::read_frame(stream).map_err(map_wire_error)?;
    let response = match serde_json::from_slice::<RpcRequest>(&body) {
        Ok(request) => handle_rpc(store, request),
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
    listener.set_nonblocking(true)?;
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let admitted =
                    peer_cred(&stream).and_then(|cred| verify_peer_uid(cred, allowed_uid));
                if let Err(error) = admitted {
                    eprintln!("pf-prefsd: peer refused: {error}");
                    continue;
                }
                if let Err(error) = stream
                    .set_nonblocking(false)
                    .and_then(|()| stream.set_read_timeout(Some(connection_timeout)))
                    .and_then(|()| stream.set_write_timeout(Some(connection_timeout)))
                {
                    eprintln!("pf-prefsd: connection setup error: {error}");
                    continue;
                }
                if let Err(error) = serve_connection(store, &mut stream) {
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

fn handle_rpc(store: &PrefsStore, request: RpcRequest) -> RpcResponse {
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
}
