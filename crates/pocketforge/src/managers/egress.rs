//! The **egress manager** — network *send* as a SEPARATE, logged, quota'd capability, distinct
//! from any data-read capability. This is the anti-exfiltration half of the owner's privacy ask:
//! `location` (read) and `egress` (send) account into different buckets, so reading a position
//! never spends send budget and vice-versa (`egress:steampowered.com` in `app.toml` is the
//! CapDL-style static grant the `.3` broker will enforce; here it is the cooperative accountant).
//!
//! Honesty (R-A): v0 does NOT open sockets or confine the network — a cooperatively-linked app
//! can still call the kernel directly. This manager proves the *accounting + audit-log shape* the
//! out-of-process broker (`.3`) enforces on the Phase-2 substrate. Every `send` appends to an
//! in-memory ledger (the AppOps-style audit trail) and decrements the `egress` bucket.

use std::sync::{Arc, Mutex};

use crate::error::CapError;

use super::QuotaLedger;

/// One recorded egress operation (the audit-log entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressReceipt {
    /// Destination host the app declared.
    pub host: String,
    /// Bytes the app reported sending.
    pub bytes: u64,
    /// `egress` allowance remaining after this send.
    pub remaining: u64,
}

/// One device-agnostic network-egress accountant.
pub struct EgressManager {
    quotas: Arc<QuotaLedger>,
    log: Mutex<Vec<EgressReceipt>>,
}

impl EgressManager {
    /// Build the manager over the session's shared quota ledger.
    pub fn new(quotas: Arc<QuotaLedger>) -> EgressManager {
        EgressManager { quotas, log: Mutex::new(Vec::new()) }
    }

    /// Remaining `egress` send allowance in this session (cooperative quota).
    pub fn remaining(&self) -> u64 {
        self.quotas.remaining("egress")
    }

    /// Account one send of `bytes` to `host`. Consumes ONE from the `egress` bucket (NEVER the
    /// `location` bucket), appends an audit entry, and returns the receipt — or `PolicyBlocked`
    /// when the egress quota is exhausted. (The actual bytes are the app's to send in v0; this is
    /// the cooperative accountant + audit log the `.3` broker turns into enforcement.)
    pub fn send(&self, host: &str, bytes: u64) -> Result<EgressReceipt, CapError> {
        if !self.quotas.try_consume("egress", 1) {
            return Err(CapError::PolicyBlocked);
        }
        let receipt = EgressReceipt {
            host: host.to_string(),
            bytes,
            remaining: self.quotas.remaining("egress"),
        };
        self.log.lock().unwrap().push(receipt.clone());
        Ok(receipt)
    }

    /// The audit trail of every accounted send this session (the AppOps-style ledger).
    pub fn audit_log(&self) -> Vec<EgressReceipt> {
        self.log.lock().unwrap().clone()
    }
}
