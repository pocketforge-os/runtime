use pf_peer_identity::ReceivedProcess;
use serde::{Deserialize, Serialize};
use std::io;

pub const MAX_REQUEST_BYTES: usize = 2048;
pub const AUTHORITY_UNIT: &str = "pf-session-authorityd.service";
pub const MENU_SIDE_UNIT: &str = "pf-input-broker.service";

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

pub fn caller_allowed(unit: &str) -> bool {
    matches!(unit, AUTHORITY_UNIT | MENU_SIDE_UNIT)
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

pub fn authorize_target(process: &ReceivedProcess, claimed: &TargetIdentity) -> io::Result<String> {
    if process.pid() != claimed.pid || proc_start_time(process.pid())? != claimed.start_time {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "target identity changed",
        ));
    }
    let cgroup = process.cgroup()?;
    let unit = pf_peer_identity::service_unit_from_cgroup(&cgroup).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "target has no allowed unit",
        )
    })?;
    if unit != claimed.unit {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "target unit mismatch",
        ));
    }
    pf_peer_identity::app_id_from_unit(&unit).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "target is not an application",
        )
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuditOutcome {
    Accepted,
    Recovered,
    RefusedCaller,
    RefusedTarget,
    InvalidRequest,
    SampleUnavailable,
}

impl AuditOutcome {
    pub const fn line(self) -> &'static str {
        match self {
            Self::Accepted => "pf-hang-sampler: result=accepted",
            Self::Recovered => "pf-hang-sampler: result=recovered",
            Self::RefusedCaller => "pf-hang-sampler: result=refused_caller",
            Self::RefusedTarget => "pf-hang-sampler: result=refused_target",
            Self::InvalidRequest => "pf-hang-sampler: result=invalid_request",
            Self::SampleUnavailable => "pf-hang-sampler: result=sample_unavailable",
        }
    }
}
