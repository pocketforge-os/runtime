use crate::protocol::{
    authorize_target, caller_allowed, decode_request, AuditOutcome, TargetIdentity, TriggerRequest,
    AUTHORITY_UNIT, MAX_REQUEST_BYTES, MENU_SIDE_UNIT,
};
use crate::report::HangHow;
use crate::{sample_target, EpisodeKey, EpisodeStore, Frame};
use std::collections::BTreeSet;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletedHang {
    pub app_id: String,
    pub how: HangHow,
    pub samples: Vec<Vec<Frame>>,
}

pub struct Sampler {
    episodes: EpisodeStore,
    allowed_build_ids: BTreeSet<Vec<u8>>,
}

impl Sampler {
    pub fn new(allowed_build_ids: BTreeSet<Vec<u8>>) -> Self {
        Self {
            episodes: EpisodeStore::default(),
            allowed_build_ids,
        }
    }

    fn key(target: &TargetIdentity, hang_nonce: u64) -> EpisodeKey {
        EpisodeKey {
            unit: target.unit.clone(),
            pid: target.pid,
            start_time: target.start_time,
            hang_nonce,
        }
    }

    fn handle_authenticated(
        &mut self,
        caller_unit: &str,
        request: TriggerRequest,
        process_fd: Option<std::os::fd::OwnedFd>,
    ) -> (AuditOutcome, Option<CompletedHang>) {
        let operation_allowed = matches!(
            (&request, caller_unit),
            (
                TriggerRequest::SuspectNoReply { .. } | TriggerRequest::Recovered { .. },
                MENU_SIDE_UNIT
            ) | (TriggerRequest::ForceStop { .. }, AUTHORITY_UNIT)
        );
        if !caller_allowed(caller_unit) || !operation_allowed {
            return (AuditOutcome::RefusedCaller, None);
        }

        match request {
            TriggerRequest::Recovered { target, hang_nonce } => {
                if process_fd.is_some() {
                    return (AuditOutcome::InvalidRequest, None);
                }
                self.episodes.recover(&Self::key(&target, hang_nonce));
                (AuditOutcome::Recovered, None)
            }
            TriggerRequest::SuspectNoReply { target, hang_nonce } => {
                let Some(fd) = process_fd else {
                    return (AuditOutcome::InvalidRequest, None);
                };
                let Ok(process) = pf_peer_identity::ReceivedProcess::from_fd(fd) else {
                    return (AuditOutcome::RefusedTarget, None);
                };
                if authorize_target(&process, &target).is_err() {
                    return (AuditOutcome::RefusedTarget, None);
                }
                let key = Self::key(&target, hang_nonce);
                if !self.episodes.contains(&key) {
                    let frames = sample_target(process.pid(), &self.allowed_build_ids);
                    self.episodes.complete(key, frames);
                }
                (AuditOutcome::Accepted, None)
            }
            TriggerRequest::ForceStop { target, .. } => {
                let Some(fd) = process_fd else {
                    return (AuditOutcome::InvalidRequest, None);
                };
                let Ok(process) = pf_peer_identity::ReceivedProcess::from_fd(fd) else {
                    return (AuditOutcome::RefusedTarget, None);
                };
                let Ok(app_id) = authorize_target(&process, &target) else {
                    return (AuditOutcome::RefusedTarget, None);
                };
                let existing =
                    self.episodes
                        .active_for_target(&target.unit, target.pid, target.start_time);
                let key = existing.clone().unwrap_or_else(|| Self::key(&target, 0));
                let frames = sample_target(process.pid(), &self.allowed_build_ids);
                self.episodes.complete(key.clone(), frames);
                let Some(samples) = self.episodes.finalize(&key) else {
                    return (AuditOutcome::Accepted, None);
                };
                (
                    AuditOutcome::Accepted,
                    Some(CompletedHang {
                        app_id,
                        how: if existing.is_some() {
                            HangHow::NoReplyMenu
                        } else {
                            HangHow::StopTimeout
                        },
                        samples,
                    }),
                )
            }
        }
    }

    pub fn handle_stream(&mut self, stream: &UnixStream) -> (AuditOutcome, Option<CompletedHang>) {
        let Ok(identity) = pf_peer_identity::peer_service_identity(stream) else {
            return (AuditOutcome::RefusedCaller, None);
        };
        let mut bytes = [0_u8; MAX_REQUEST_BYTES + 1];
        let Ok((length, process_fd)) =
            pf_peer_identity::scm::recv_with_fd(stream.as_raw_fd(), &mut bytes)
        else {
            return (AuditOutcome::InvalidRequest, None);
        };
        let Ok(request) = decode_request(&bytes[..length]) else {
            return (AuditOutcome::InvalidRequest, None);
        };
        self.handle_authenticated(&identity.unit, request, process_fd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{caller_allowed, AuditOutcome};

    #[test]
    fn zero_pii_canary_and_exact_caller_policy() {
        assert!(caller_allowed(AUTHORITY_UNIT));
        assert!(caller_allowed(MENU_SIDE_UNIT));
        for unit in [
            "pf-osk.service",
            "pf-fcitx5.service",
            "pf-menu.service",
            "pf-input-broker.service.evil",
        ] {
            assert!(!caller_allowed(unit));
        }

        let canaries = [
            "/home/alice/private/game",
            "register-rip-deadbeef",
            "stack-bytes-secret",
        ];
        for outcome in [
            AuditOutcome::Accepted,
            AuditOutcome::Recovered,
            AuditOutcome::RefusedCaller,
            AuditOutcome::RefusedTarget,
            AuditOutcome::InvalidRequest,
            AuditOutcome::SampleUnavailable,
        ] {
            for canary in canaries {
                assert!(!outcome.line().contains(canary));
            }
        }

        for forbidden in ["path", "registers", "stack_bytes"] {
            let request = format!(
                "{{\"op\":\"recovered\",\"target\":{{\"unit\":\"pf-app@org.example.hung.service\",\"pid\":1,\"start_time\":2}},\"hang_nonce\":3,\"{forbidden}\":\"stack-bytes-secret\"}}"
            );
            assert!(crate::protocol::decode_request(request.as_bytes()).is_err());
        }
    }
}
