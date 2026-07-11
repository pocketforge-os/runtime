//! The **egress manager** — network *send* as a SEPARATE, logged, quota'd capability, distinct
//! from any data-read capability. This is the anti-exfiltration half of the owner's privacy ask:
//! `location` (read) and `egress` (send) account into different buckets, so reading a position
//! never spends send budget and vice-versa. `egress:<host>` in `app.toml` is the CapDL-style
//! static grant; here it is the cooperative accountant + the per-host byte ledger that landed
//! in `tsp-ht0p.4`.
//!
//! ## What `.4` added on top of `.3`'s cooperative shape
//!
//! * **Wall-clock TOKEN BUCKET** for the `egress` op count — refills at the tier-default rate
//!   (`super::EGRESS_REFILL_PER_SEC`) so a burst spike drains + then heals as time passes,
//!   replacing the plain counter that decremented to zero forever. Threading is unchanged; the
//!   manager still calls `super::QuotaLedger::try_consume`.
//! * **Declared-host allowlist** — pass the manifest's `egress_hosts()` set into
//!   [`EgressAccounting::with_declared_hosts`] and a `send` to an UNDECLARED host is REFUSED
//!   (returns [`CapError::PolicyBlocked`] and appends a `refused` row to the persistent log)
//!   without touching any bucket. Match against declared strings is byte-equal (the manifest's
//!   host token is verbatim); wildcards resolve at the tier-classifier layer and reject broad
//!   egress from an untrusted app at LAUNCH ([`pf_broker::Tier::Signature`]).
//! * **Per-host byte accounting** — declared sends append a `send` row to the persistent log
//!   (same JSONL-ish dialect as `.3`'s AppOps ledger) so `pf-permissions egress` can roll a
//!   per-host byte total + refusal count. Byte-cap enforcement is out of scope for v1 (accounting
//!   only per the Q1 ruling); the log is the surface a future per-host byte cap keys off.
//!
//! Honesty (R-A): v1 = accounting + declaration + quota CONTRACT. Every event here is
//! COOPERATIVE — a linked app can still call the kernel directly. The enforcement seam that
//! closes that gap is designed in `docs/EGRESS-ENFORCEMENT-SEAM.md` and tracked by the follow-on
//! bead filed on merge. Backward-compat: [`EgressManager::new`] preserves the legacy
//! declaration-agnostic shape (any host allowed, in-memory audit only) so every merged
//! consumer — the STEP-2 transcript, `Pf::egress`, and the `.3`-era location-read-vs-egress
//! independence test — keeps working. Declaration + persistent logging is opt-in through the
//! new [`EgressAccounting`] builder.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use crate::error::CapError;

use super::egress_log::{EgressEvent, EgressLog};
use super::QuotaLedger;

/// One recorded egress operation — the in-memory audit-log entry (parallel to the persistent
/// [`crate::managers::egress_log::EgressEvent`], which is what `pf-permissions egress` reads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressReceipt {
    /// Destination host the app declared.
    pub host: String,
    /// Bytes the app reported sending.
    pub bytes: u64,
    /// `egress` op-token allowance remaining after this send.
    pub remaining: u64,
}

/// Opt-in declaration + persistent-log wiring for [`EgressManager`]. Built with
/// [`EgressManager::accounting`] and the manifest's declared host set + app id + a shared
/// [`EgressLog`].
#[derive(Clone)]
pub struct EgressAccounting {
    /// Application id (the manifest's `[app].id`) — used as the ledger row key.
    pub app_id: String,
    /// Hosts declared by the manifest's `egress:<host>` entries. A send to a host outside this
    /// set is refused.
    pub declared_hosts: BTreeSet<String>,
    /// The persistent per-host log (shared across managers in the same session).
    pub log: Arc<EgressLog>,
}

/// One device-agnostic network-egress accountant.
pub struct EgressManager {
    quotas: Arc<QuotaLedger>,
    // In-memory audit log (kept for backward-compat: the merged `.3`-era test asserts
    // `audit_log()` after a send). Also mirrored to the persistent `EgressLog` when the
    // accounting-wiring is opt-in-installed.
    log: Mutex<Vec<EgressReceipt>>,
    accounting: Option<EgressAccounting>,
}

impl EgressManager {
    /// The **legacy / co-op** constructor: no declaration set, no persistent log — any host is
    /// allowed and only the in-memory `audit_log()` is kept. Preserves every merged consumer
    /// (`Pf::egress`, `.2`/`.3` tests, the STEP-2 transcript). New code paths that want
    /// declaration + persistent logging call [`EgressManager::with_accounting`].
    pub fn new(quotas: Arc<QuotaLedger>) -> EgressManager {
        EgressManager {
            quotas,
            log: Mutex::new(Vec::new()),
            accounting: None,
        }
    }

    /// The **`.4` declared+logged** constructor: the manifest's declared host set is the
    /// declared-host allowlist; every accounted `send` AND every refused undeclared send lands
    /// in the persistent [`EgressLog`] under `<app_id>.log`. Backward-compat helper name
    /// intentionally distinct from [`new`](Self::new).
    pub fn with_accounting(quotas: Arc<QuotaLedger>, accounting: EgressAccounting) -> EgressManager {
        EgressManager {
            quotas,
            log: Mutex::new(Vec::new()),
            accounting: Some(accounting),
        }
    }

    /// Convenience builder for the [`EgressAccounting`] payload from an app id + declared host
    /// iterator + a shared [`EgressLog`].
    pub fn accounting<S: Into<String>>(
        app_id: impl Into<String>,
        hosts: impl IntoIterator<Item = S>,
        log: Arc<EgressLog>,
    ) -> EgressAccounting {
        EgressAccounting {
            app_id: app_id.into(),
            declared_hosts: hosts.into_iter().map(|h| h.into()).collect(),
            log,
        }
    }

    /// Remaining `egress` op-token allowance (from the shared wall-clock bucket). Refills at
    /// [`super::EGRESS_REFILL_PER_SEC`] until saturated at [`super::EGRESS_QUOTA`].
    pub fn remaining(&self) -> u64 {
        self.quotas.remaining("egress")
    }

    /// Is `host` in the declared set? Always `true` when the manager was built via
    /// [`new`](Self::new) (legacy / co-op mode: no declaration enforcement).
    pub fn is_declared(&self, host: &str) -> bool {
        match &self.accounting {
            None => true,
            Some(a) => a.declared_hosts.contains(host),
        }
    }

    /// Account one send of `bytes` to `host`.
    ///
    /// Ordering:
    ///   1. **Declared-host check** (when `accounting` is wired). Undeclared ⇒
    ///      [`CapError::PolicyBlocked`] with a `refused` row appended to the log; NO bucket
    ///      touched (the app spent no budget on a refused op).
    ///   2. **Op token bucket**. `egress` bucket refills over wall time; drained ⇒
    ///      [`CapError::PolicyBlocked`].
    ///   3. **Persist + return the receipt** — appends a `send` row to the log (when
    ///      accounting is wired) and always returns the in-memory receipt.
    pub fn send(&self, host: &str, bytes: u64) -> Result<EgressReceipt, CapError> {
        // (1) declared-host gate — logs a `refused` row without spending a token.
        if let Some(a) = &self.accounting {
            if !a.declared_hosts.contains(host) {
                // Log the refusal (best-effort; a log-io failure must not silently swallow the
                // policy refusal — surface PolicyBlocked either way).
                let remaining = self.quotas.remaining("egress");
                let _ = a.log.record_refused(
                    &a.app_id,
                    host,
                    format!("host {host} not declared in use=[]"),
                    remaining,
                );
                return Err(CapError::PolicyBlocked);
            }
        }
        // (2) op quota (wall-clock bucket).
        if !self.quotas.try_consume("egress", 1) {
            if let Some(a) = &self.accounting {
                let remaining = self.quotas.remaining("egress");
                let _ = a.log.record_refused(&a.app_id, host, "egress op quota exhausted", remaining);
            }
            return Err(CapError::PolicyBlocked);
        }
        // (3) success: build the receipt + persist.
        let remaining = self.quotas.remaining("egress");
        let receipt = EgressReceipt {
            host: host.to_string(),
            bytes,
            remaining,
        };
        self.log.lock().unwrap().push(receipt.clone());
        if let Some(a) = &self.accounting {
            let _ = a.log.record_send(&a.app_id, host, bytes, remaining);
        }
        Ok(receipt)
    }

    /// The in-memory audit trail of every accounted send this session (preserved for the
    /// `.3`-era location-read-vs-egress independence test that asserts `audit_log().len()`).
    pub fn audit_log(&self) -> Vec<EgressReceipt> {
        self.log.lock().unwrap().clone()
    }

    /// The persistent-log events for this app id (from disk). Empty when no accounting is
    /// wired. Used by the STEP-3 harness + `pf-permissions egress`.
    pub fn persistent_events(&self) -> Vec<EgressEvent> {
        match &self.accounting {
            None => Vec::new(),
            Some(a) => a.log.snapshot_for(&a.app_id).unwrap_or_default(),
        }
    }
}
