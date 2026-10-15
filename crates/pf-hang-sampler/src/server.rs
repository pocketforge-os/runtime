use crate::protocol::{
    authorize_target, caller_allowed, AuditOutcome, AUTHORITY_UNIT, MENU_SIDE_UNIT,
};
use crate::report::HangHow;
use crate::{prepared_worker_ttl, sample_target, EpisodeKey, EpisodeStore, Frame, PreparedSample};
use pf_hang_trigger::{decode_request, TargetIdentity, TriggerRequest, MAX_REQUEST_BYTES};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::Instant;

const MAX_PREPARED_TARGETS: usize = 128;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PreparedKey {
    session_id: String,
    item_id: String,
}

struct PreparedTarget {
    target: TargetIdentity,
    app_id: String,
    process: pf_peer_identity::ReceivedProcess,
    worker: PreparedSample,
    expires_at: Instant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletedHang {
    pub app_id: String,
    pub how: HangHow,
    pub samples: Vec<Vec<Frame>>,
}

pub struct Sampler {
    episodes: EpisodeStore,
    allowed_build_ids: BTreeSet<Vec<u8>>,
    prepared: BTreeMap<PreparedKey, PreparedTarget>,
    prepared_order: VecDeque<PreparedKey>,
}

impl Sampler {
    pub fn new(allowed_build_ids: BTreeSet<Vec<u8>>) -> Self {
        Self {
            episodes: EpisodeStore::default(),
            allowed_build_ids,
            prepared: BTreeMap::new(),
            prepared_order: VecDeque::new(),
        }
    }

    fn purge_prepared(&mut self) {
        let now = Instant::now();
        self.prepared.retain(|_, entry| entry.expires_at > now);
        self.prepared_order
            .retain(|key| self.prepared.contains_key(key));
    }

    fn insert_prepared(&mut self, key: PreparedKey, entry: PreparedTarget) {
        self.prepared_order.retain(|existing| existing != &key);
        self.prepared.insert(key.clone(), entry);
        self.prepared_order.push_back(key);
        while self.prepared_order.len() > MAX_PREPARED_TARGETS {
            if let Some(expired) = self.prepared_order.pop_front() {
                self.prepared.remove(&expired);
            }
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
        self.purge_prepared();
        let operation_allowed = matches!(
            (&request, caller_unit),
            (
                TriggerRequest::SuspectNoReply { .. } | TriggerRequest::Recovered { .. },
                MENU_SIDE_UNIT
            ) | (
                TriggerRequest::Prepare { .. }
                    | TriggerRequest::ForceStop { .. }
                    | TriggerRequest::DiscardPrepared { .. },
                AUTHORITY_UNIT
            )
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
            TriggerRequest::Prepare {
                target,
                session_id,
                grace_ms,
            } => {
                let Some(fd) = process_fd else {
                    return (AuditOutcome::InvalidRequest, None);
                };
                let Ok(process) = pf_peer_identity::ReceivedProcess::from_fd(fd) else {
                    return (AuditOutcome::RefusedTarget, None);
                };
                let Ok(app_id) = authorize_target(&process, &target) else {
                    return (AuditOutcome::RefusedTarget, None);
                };
                let Ok(ttl) = prepared_worker_ttl(grace_ms) else {
                    return (AuditOutcome::InvalidRequest, None);
                };
                let Ok(worker) = PreparedSample::prepare(
                    process.pid(),
                    target.start_time,
                    ttl,
                    &self.allowed_build_ids,
                ) else {
                    return (AuditOutcome::SampleUnavailable, None);
                };
                self.insert_prepared(
                    PreparedKey {
                        session_id,
                        item_id: app_id.clone(),
                    },
                    PreparedTarget {
                        target,
                        app_id,
                        process,
                        worker,
                        expires_at: Instant::now() + ttl,
                    },
                );
                (AuditOutcome::Accepted, None)
            }
            TriggerRequest::ForceStop {
                item_id,
                session_id,
                ..
            } => {
                if process_fd.is_some() || pf_app_manifest::validate_app_id(&item_id).is_err() {
                    return (AuditOutcome::InvalidRequest, None);
                }
                let key = PreparedKey {
                    session_id,
                    item_id: item_id.clone(),
                };
                let Some(prepared) = self.prepared.remove(&key) else {
                    return (
                        AuditOutcome::SampleUnavailable,
                        Some(CompletedHang {
                            app_id: item_id,
                            how: HangHow::StopTimeout,
                            samples: Vec::new(),
                        }),
                    );
                };
                self.prepared_order.retain(|entry| entry != &key);
                let PreparedTarget {
                    target,
                    app_id,
                    process,
                    worker,
                    ..
                } = prepared;
                let Ok(current_app_id) = authorize_target(&process, &target) else {
                    return (AuditOutcome::RefusedTarget, None);
                };
                if current_app_id != app_id {
                    return (AuditOutcome::RefusedTarget, None);
                }
                let existing =
                    self.episodes
                        .active_for_target(&target.unit, target.pid, target.start_time);
                let key = existing.clone().unwrap_or_else(|| Self::key(&target, 0));
                let frames = worker.sample();
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
            TriggerRequest::DiscardPrepared {
                item_id,
                session_id,
            } => {
                if process_fd.is_some() || pf_app_manifest::validate_app_id(&item_id).is_err() {
                    return (AuditOutcome::InvalidRequest, None);
                }
                let key = PreparedKey {
                    session_id,
                    item_id,
                };
                self.prepared.remove(&key);
                self.prepared_order.retain(|entry| entry != &key);
                (AuditOutcome::Recovered, None)
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
    use pf_hang_trigger::ForceStopReason;

    #[test]
    fn force_stop_without_prepare_has_no_pinned_target_and_is_empty() {
        let mut sampler = Sampler::new(BTreeSet::new());
        let (outcome, report) = sampler.handle_authenticated(
            AUTHORITY_UNIT,
            TriggerRequest::ForceStop {
                item_id: "org.example.game".to_owned(),
                session_id: "restart-lost-deadline".to_owned(),
                reason: ForceStopReason::StopTimeout,
            },
            None,
        );

        assert_eq!(outcome, AuditOutcome::SampleUnavailable);
        assert!(sampler.prepared.is_empty());
        assert_eq!(report.unwrap().samples, Vec::<Vec<Frame>>::new());
    }

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
            assert!(pf_hang_trigger::decode_request(request.as_bytes()).is_err());
        }
    }
}
