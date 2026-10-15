use pf_hang_trigger::{proc_start_time, TargetIdentity};
use pf_peer_identity::ReceivedProcess;
use std::io;

pub const AUTHORITY_UNIT: &str = "pf-session-authorityd.service";
pub const MENU_SIDE_UNIT: &str = "pf-input-broker.service";

pub fn caller_allowed(unit: &str) -> bool {
    matches!(unit, AUTHORITY_UNIT | MENU_SIDE_UNIT)
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
