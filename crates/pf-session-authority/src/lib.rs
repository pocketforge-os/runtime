//! Independent, transport-neutral foreground session authority.
//!
//! A terminal receipt is truthful only after this observation ladder completes in order:
//! foreground unit inactive, foreground target released, selected shell owner active, and a
//! real shell presentation acknowledged. The core atomically persists the completed receipt
//! before publishing it. Failure at termination or at any ladder rung atomically records
//! [`RecoveryRequired`] instead; an unavailable shell is never asked to render its own failure.

use pf_ports::{
    Clock, LaunchRequest, LaunchResult, MonotonicTime, ObservedSessionState, RecoveryRequired,
    SessionEvent, TerminalReceipt,
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthorityError {
    Backend(String),
    Persistence(String),
    InvalidObservation,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Receipt {
    Returned,
    ForcedClose,
    Crash { summary: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub session_id: String,
    pub item_id: String,
    pub receipt: Option<Receipt>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Phase {
    Idle,
    Starting {
        session_id: String,
    },
    Running {
        session_id: String,
    },
    StoppingGracefully {
        session_id: String,
        deadline_nanos: u64,
    },
    ForceStopping {
        session_id: String,
    },
    Restoring {
        session_id: String,
        receipt: Receipt,
        rung: RestorationRung,
    },
    RecoveryRequired {
        session_id: String,
        reason: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RestorationRung {
    UnitInactive,
    TargetReleased,
    OwnerActive,
    PresentationAcknowledged,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct PublishedEvent {
    sequence: u64,
    event: WireEvent,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum WireEvent {
    ObservedStarting,
    ObservedRunning,
    ObservationComplete,
    Terminal {
        session_id: String,
        receipt: Receipt,
    },
    RecoveryRequired {
        session_id: String,
        reason: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PersistedState {
    pub phase: Phase,
    pub history: VecDeque<HistoryEntry>,
    pending: VecDeque<PublishedEvent>,
    next_sequence: u64,
    next_session: u64,
    safe_return_queue: u64,
    pub safe_return_binding_revision: u64,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            history: VecDeque::new(),
            pending: VecDeque::new(),
            next_sequence: 1,
            next_session: 1,
            safe_return_queue: 0,
            safe_return_binding_revision: 0,
        }
    }
}

pub trait StateStore {
    fn load(&self) -> Result<Option<PersistedState>, AuthorityError>;
    fn save(&mut self, state: &PersistedState) -> Result<(), AuthorityError>;
}

#[derive(Clone, Debug, Default)]
pub struct MemoryStore {
    state: Option<PersistedState>,
}
impl MemoryStore {
    pub fn snapshot(&self) -> Option<&PersistedState> {
        self.state.as_ref()
    }
}
impl StateStore for MemoryStore {
    fn load(&self) -> Result<Option<PersistedState>, AuthorityError> {
        Ok(self.state.clone())
    }
    fn save(&mut self, state: &PersistedState) -> Result<(), AuthorityError> {
        self.state = Some(state.clone());
        Ok(())
    }
}

/// JSON state store using write-and-rename atomic replacement in the destination directory.
pub struct FileStore {
    path: PathBuf,
}
impl FileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}
impl StateStore for FileStore {
    fn load(&self) -> Result<Option<PersistedState>, AuthorityError> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| AuthorityError::Persistence(e.to_string())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(AuthorityError::Persistence(e.to_string())),
        }
    }
    fn save(&mut self, state: &PersistedState) -> Result<(), AuthorityError> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|e| AuthorityError::Persistence(e.to_string()))?;
        let tmp = self.path.with_extension("tmp");
        let bytes =
            serde_json::to_vec(state).map_err(|e| AuthorityError::Persistence(e.to_string()))?;
        let result = (|| -> io::Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&tmp, &self.path)?;
            fs::File::open(parent)?.sync_all()
        })();
        result.map_err(|e| AuthorityError::Persistence(e.to_string()))
    }
}

/// Trait-shaped image/service integration. F13 supplies the real systemd implementation.
pub trait SessionSystem {
    fn start_foreground(
        &mut self,
        request: &LaunchRequest,
        session_id: &str,
    ) -> Result<bool, String>;
    fn request_graceful_stop(&mut self, session_id: &str) -> Result<(), String>;
    fn enforce_termination(&mut self, session_id: &str) -> Result<(), String>;
    fn activate_selected_owner(&mut self) -> Result<(), String>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Observation {
    SessionRunning,
    SessionExitedCleanly,
    SessionCrashed { summary: String },
    UnitInactive,
    TargetReleased,
    SelectedOwnerActive,
    PresentationAcknowledged,
    Failed { rung: FailureRung, reason: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureRung {
    Termination,
    UnitInactive,
    TargetReleased,
    OwnerActivation,
    OwnerActive,
    Presentation,
}

pub trait AuthorityApi {
    fn launch(&mut self, request: LaunchRequest) -> Result<LaunchResult, AuthorityError>;
    fn events_after(&self, sequence: u64) -> Vec<(u64, SessionEvent)>;
    fn history(&self) -> Vec<SessionEvent>;
}

pub struct Authority<S, B, C> {
    store: S,
    system: B,
    clock: C,
    state: PersistedState,
    recent_bound: usize,
    grace: Duration,
}

impl<S: StateStore, B: SessionSystem, C: Clock> Authority<S, B, C> {
    pub fn open(
        store: S,
        system: B,
        clock: C,
        recent_bound: usize,
        grace: Duration,
    ) -> Result<Self, AuthorityError> {
        let state = store.load()?.unwrap_or_default();
        Ok(Self {
            store,
            system,
            clock,
            state,
            recent_bound: recent_bound.max(1),
            grace,
        })
    }
    pub fn state(&self) -> &PersistedState {
        &self.state
    }
    pub fn into_parts(self) -> (S, B, C) {
        (self.store, self.system, self.clock)
    }
    fn persist(&mut self) -> Result<(), AuthorityError> {
        self.store.save(&self.state)
    }
    fn session_id(&self) -> Option<String> {
        match &self.state.phase {
            Phase::Idle => None,
            Phase::Starting { session_id }
            | Phase::Running { session_id }
            | Phase::StoppingGracefully { session_id, .. }
            | Phase::ForceStopping { session_id }
            | Phase::Restoring { session_id, .. }
            | Phase::RecoveryRequired { session_id, .. } => Some(session_id.clone()),
        }
    }
    fn publish(&mut self, event: WireEvent) {
        let sequence = self.state.next_sequence;
        self.state.next_sequence += 1;
        self.state
            .pending
            .push_back(PublishedEvent { sequence, event });
    }
    fn recover(&mut self, reason: String) -> Result<(), AuthorityError> {
        let session_id = self
            .session_id()
            .ok_or(AuthorityError::InvalidObservation)?;
        self.state.phase = Phase::RecoveryRequired {
            session_id: session_id.clone(),
            reason: reason.clone(),
        };
        self.publish(WireEvent::RecoveryRequired { session_id, reason });
        self.persist()
    }
    pub fn update_safe_return_binding(&mut self, revision: u64) -> Result<(), AuthorityError> {
        if revision > self.state.safe_return_binding_revision {
            self.state.safe_return_binding_revision = revision;
            self.persist()?;
        }
        Ok(())
    }
    /// Durable protected intake, deliberately separate from any foreground application's input.
    pub fn intake_safe_return(&mut self) -> Result<(), AuthorityError> {
        self.state.safe_return_queue = self.state.safe_return_queue.saturating_add(1);
        self.persist()
    }
    pub fn reconcile(&mut self) -> Result<(), AuthorityError> {
        if self.state.safe_return_queue > 0
            && matches!(
                self.state.phase,
                Phase::Starting { .. } | Phase::Running { .. }
            )
        {
            self.state.safe_return_queue -= 1;
            let id = self.session_id().unwrap();
            let deadline_nanos = self.clock.deadline_after(self.grace).0.as_nanos();
            match self.system.request_graceful_stop(&id) {
                Ok(()) => {
                    self.state.phase = Phase::StoppingGracefully {
                        session_id: id,
                        deadline_nanos,
                    }
                }
                Err(_) => {
                    if let Err(reason) = self.system.enforce_termination(&id) {
                        return self.recover(format!("termination: {reason}"));
                    }
                    self.state.phase = Phase::ForceStopping { session_id: id };
                }
            }
            self.persist()?;
        }
        Ok(())
    }
    pub fn tick(&mut self) -> Result<(), AuthorityError> {
        self.reconcile()?;
        if let Phase::StoppingGracefully {
            session_id,
            deadline_nanos,
        } = self.state.phase.clone()
        {
            if self.clock.now() >= MonotonicTime::from_nanos(deadline_nanos) {
                if let Err(reason) = self.system.enforce_termination(&session_id) {
                    return self.recover(format!("termination: {reason}"));
                }
                self.state.phase = Phase::ForceStopping { session_id };
                self.persist()?;
            }
        }
        Ok(())
    }
    fn begin_restoration(&mut self, receipt: Receipt) -> Result<(), AuthorityError> {
        let session_id = self
            .session_id()
            .ok_or(AuthorityError::InvalidObservation)?;
        self.state.phase = Phase::Restoring {
            session_id,
            receipt,
            rung: RestorationRung::UnitInactive,
        };
        self.persist()
    }
    fn unit_inactive(&mut self, receipt: Receipt) -> Result<(), AuthorityError> {
        let session_id = self
            .session_id()
            .ok_or(AuthorityError::InvalidObservation)?;
        self.state.phase = Phase::Restoring {
            session_id,
            receipt,
            rung: RestorationRung::TargetReleased,
        };
        self.persist()
    }
    pub fn observe(&mut self, observation: Observation) -> Result<(), AuthorityError> {
        if let Observation::Failed { rung, reason } = observation {
            return self.recover(format!("{rung:?}: {reason}"));
        }
        match (&self.state.phase, observation) {
            (Phase::Starting { .. }, Observation::SessionRunning) => {
                let id = self.session_id().unwrap();
                self.state.phase = Phase::Running { session_id: id };
                self.publish(WireEvent::ObservedRunning);
                self.persist()
            }
            (Phase::Starting { .. } | Phase::Running { .. }, Observation::SessionExitedCleanly) => {
                self.begin_restoration(Receipt::Returned)
            }
            (
                Phase::Starting { .. } | Phase::Running { .. },
                Observation::SessionCrashed { summary },
            ) => self.begin_restoration(Receipt::Crash { summary }),
            (Phase::StoppingGracefully { .. }, Observation::UnitInactive) => {
                self.unit_inactive(Receipt::Returned)
            }
            (Phase::ForceStopping { .. }, Observation::UnitInactive) => {
                self.unit_inactive(Receipt::ForcedClose)
            }
            (
                Phase::Restoring {
                    session_id,
                    receipt,
                    rung: RestorationRung::UnitInactive,
                },
                Observation::UnitInactive,
            ) => {
                self.state.phase = Phase::Restoring {
                    session_id: session_id.clone(),
                    receipt: receipt.clone(),
                    rung: RestorationRung::TargetReleased,
                };
                self.persist()
            }
            (
                Phase::Restoring {
                    session_id,
                    receipt,
                    rung: RestorationRung::TargetReleased,
                },
                Observation::TargetReleased,
            ) => {
                let session_id = session_id.clone();
                let receipt = receipt.clone();
                if let Err(reason) = self.system.activate_selected_owner() {
                    return self.recover(format!("owner activation: {reason}"));
                }
                self.state.phase = Phase::Restoring {
                    session_id,
                    receipt,
                    rung: RestorationRung::OwnerActive,
                };
                self.persist()
            }
            (
                Phase::Restoring {
                    session_id,
                    receipt,
                    rung: RestorationRung::OwnerActive,
                },
                Observation::SelectedOwnerActive,
            ) => {
                self.state.phase = Phase::Restoring {
                    session_id: session_id.clone(),
                    receipt: receipt.clone(),
                    rung: RestorationRung::PresentationAcknowledged,
                };
                self.persist()
            }
            (
                Phase::Restoring {
                    session_id,
                    receipt,
                    rung: RestorationRung::PresentationAcknowledged,
                },
                Observation::PresentationAcknowledged,
            ) => {
                let id = session_id.clone();
                let receipt = receipt.clone();
                if let Some(entry) = self.state.history.iter_mut().find(|e| e.session_id == id) {
                    entry.receipt = Some(receipt.clone());
                }
                self.publish(WireEvent::ObservationComplete);
                self.publish(WireEvent::Terminal {
                    session_id: id,
                    receipt,
                });
                self.state.phase = Phase::Idle;
                self.persist()
            }
            _ => Err(AuthorityError::InvalidObservation),
        }
    }
}

impl<S: StateStore, B: SessionSystem, C: Clock> AuthorityApi for Authority<S, B, C> {
    fn launch(&mut self, request: LaunchRequest) -> Result<LaunchResult, AuthorityError> {
        if !matches!(self.state.phase, Phase::Idle) {
            return Ok(LaunchResult::RejectedBusy);
        }
        let id = format!("session-{}", self.state.next_session);
        self.state.next_session += 1;
        if !self
            .system
            .start_foreground(&request, &id)
            .map_err(AuthorityError::Backend)?
        {
            return Ok(LaunchResult::ItemUnavailable);
        }
        self.state.phase = Phase::Starting {
            session_id: id.clone(),
        };
        self.state.history.push_front(HistoryEntry {
            session_id: id.clone(),
            item_id: request.item_id,
            receipt: None,
        });
        self.state.history.truncate(self.recent_bound);
        self.publish(WireEvent::ObservedStarting);
        self.persist()?;
        Ok(LaunchResult::Accepted { session_id: id })
    }
    fn events_after(&self, sequence: u64) -> Vec<(u64, SessionEvent)> {
        self.state
            .pending
            .iter()
            .filter(|e| e.sequence > sequence)
            .map(|e| (e.sequence, wire_to_port(&e.event)))
            .collect()
    }
    fn history(&self) -> Vec<SessionEvent> {
        self.state
            .history
            .iter()
            .filter_map(|h| {
                h.receipt
                    .as_ref()
                    .map(|r| SessionEvent::Terminal(receipt_to_port(r, &h.session_id)))
            })
            .collect()
    }
}

fn receipt_to_port(receipt: &Receipt, id: &str) -> TerminalReceipt {
    match receipt {
        Receipt::Returned => TerminalReceipt::Returned {
            session_id: id.into(),
        },
        Receipt::ForcedClose => TerminalReceipt::ForcedClose {
            session_id: id.into(),
        },
        Receipt::Crash { summary } => TerminalReceipt::Crash {
            session_id: id.into(),
            summary: summary.clone(),
        },
    }
}
fn wire_to_port(event: &WireEvent) -> SessionEvent {
    match event {
        WireEvent::ObservedStarting => SessionEvent::Observed(ObservedSessionState::Starting),
        WireEvent::ObservedRunning => SessionEvent::Observed(ObservedSessionState::Running),
        WireEvent::ObservationComplete => {
            SessionEvent::Observed(ObservedSessionState::ObservationComplete)
        }
        WireEvent::Terminal {
            session_id,
            receipt,
        } => SessionEvent::Terminal(receipt_to_port(receipt, session_id)),
        WireEvent::RecoveryRequired { session_id, reason } => {
            SessionEvent::RecoveryRequired(RecoveryRequired {
                session_id: session_id.clone(),
                reason: reason.clone(),
            })
        }
    }
}

#[cfg(test)]
mod tests;
