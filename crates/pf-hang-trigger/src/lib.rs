//! Dependency-light wire contract and client for requesting a hang sample.

use serde::{Deserialize, Serialize};
use std::io;
use std::time::Duration;

pub mod client;
mod transport;

pub const MAX_REQUEST_BYTES: usize = 2048;
pub const MAX_PREPARED_GRACE: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetIdentity {
    pub unit: String,
    pub pid: i32,
    pub start_time: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForceStopReason {
    StopTimeout,
    StopRequestFailed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum TriggerRequest {
    SuspectNoReply {
        target: TargetIdentity,
        hang_nonce: u64,
    },
    Recovered {
        target: TargetIdentity,
        hang_nonce: u64,
    },
    Prepare {
        target: TargetIdentity,
        session_id: String,
        grace_ms: u64,
    },
    ForceStop {
        item_id: String,
        session_id: String,
        reason: ForceStopReason,
    },
    DiscardPrepared {
        item_id: String,
        session_id: String,
    },
}

pub fn encode_request(request: &TriggerRequest) -> io::Result<Vec<u8>> {
    let bytes = serde_json::to_vec(request).map_err(io::Error::other)?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "hang trigger exceeds wire limit",
        ));
    }
    Ok(bytes)
}

pub fn decode_request(bytes: &[u8]) -> io::Result<TriggerRequest> {
    if bytes.is_empty() || bytes.len() > MAX_REQUEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid hang trigger length",
        ));
    }
    serde_json::from_slice(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid hang trigger"))
}

pub fn proc_start_time(pid: i32) -> io::Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (_, fields) = stat.rsplit_once(") ").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "target stat has no command boundary",
        )
    })?;
    fields
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "target stat is truncated"))?
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid target start time"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_request_wire_bytes_are_stable() {
        let target = TargetIdentity {
            unit: "pf-app@org.example.game.service".to_owned(),
            pid: 42,
            start_time: 99,
        };
        let cases = [
            (
                TriggerRequest::SuspectNoReply {
                    target: target.clone(),
                    hang_nonce: 7,
                },
                r#"{"op":"suspect_no_reply","target":{"unit":"pf-app@org.example.game.service","pid":42,"start_time":99},"hang_nonce":7}"#,
            ),
            (
                TriggerRequest::Recovered {
                    target: target.clone(),
                    hang_nonce: 7,
                },
                r#"{"op":"recovered","target":{"unit":"pf-app@org.example.game.service","pid":42,"start_time":99},"hang_nonce":7}"#,
            ),
            (
                TriggerRequest::Prepare {
                    target,
                    session_id: "session-1".to_owned(),
                    grace_ms: 1500,
                },
                r#"{"op":"prepare","target":{"unit":"pf-app@org.example.game.service","pid":42,"start_time":99},"session_id":"session-1","grace_ms":1500}"#,
            ),
            (
                TriggerRequest::ForceStop {
                    item_id: "org.example.game".to_owned(),
                    session_id: "session-1".to_owned(),
                    reason: ForceStopReason::StopTimeout,
                },
                r#"{"op":"force_stop","item_id":"org.example.game","session_id":"session-1","reason":"stop_timeout"}"#,
            ),
            (
                TriggerRequest::DiscardPrepared {
                    item_id: "org.example.game".to_owned(),
                    session_id: "session-1".to_owned(),
                },
                r#"{"op":"discard_prepared","item_id":"org.example.game","session_id":"session-1"}"#,
            ),
        ];

        for (request, golden) in cases {
            let encoded = encode_request(&request).unwrap();
            assert_eq!(encoded, golden.as_bytes());
            assert_eq!(decode_request(&encoded).unwrap(), request);
        }
    }
}
