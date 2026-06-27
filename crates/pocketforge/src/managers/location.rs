//! The **location manager** — GNSS, the privacy **dangerous tier**: default-deny, consent-gated
//! (E3), and rate-accounted into the `location` quota bucket. Crucially, **location-read is a
//! DIFFERENT capability from location-send**: reading a fix consumes the `location` bucket here;
//! transmitting it off-device is the separate, tighter-quota'd, logged [`super::egress`]
//! capability. An app may be allowed to read its position for a local map yet never allowed to
//! exfiltrate it.
//!
//! Neither shipping device advertises GNSS today (a133 has none; a523's GNSS is DT-but-unbound,
//! so the E1 descriptor OMITS it). So on real hardware `read_fix` is honestly `HardwareAbsent`;
//! the default-deny/consent/quota state machine is real code exercised via a SYNTHETIC GNSS
//! descriptor (cross-repo follow-up `tsp-9sx.6` reconciles the schema's `gnss` sensor kind).

use std::sync::Arc;

use crate::backend::Backend;
use crate::error::{CapError, PermissionState};

use super::{reconcile_presence, HardwareProbe, QuotaLedger};

/// A device-agnostic position fix (WGS-84). Cooperative/placeholder values in v0 — there is no
/// real GNSS silicon to read; this proves the contract + accounting, not a real position.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Fix {
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub alt_m: f64,
    /// Horizontal accuracy estimate (m); `f64::INFINITY` until real silicon.
    pub accuracy_m: f64,
}

/// One device-agnostic location object.
pub struct LocationManager {
    backend: Arc<dyn Backend>,
    probe: Arc<dyn HardwareProbe>,
    quotas: Arc<QuotaLedger>,
}

impl LocationManager {
    /// Build the manager from a session's backend + probe + the shared quota ledger.
    pub fn new(
        backend: Arc<dyn Backend>,
        probe: Arc<dyn HardwareProbe>,
        quotas: Arc<QuotaLedger>,
    ) -> LocationManager {
        LocationManager { backend, probe, quotas }
    }

    /// Is GNSS present (descriptor ∧ ¬probe-demoted)?
    pub fn present(&self) -> bool {
        reconcile_presence(self.backend.is_present("location"), &*self.probe, "location")
    }

    /// The side-effect-free permission state (`Granted` / `Denied` / `Prompt`). Default-deny ⇒
    /// `Prompt` when present-but-undecided, `Denied` when absent.
    pub fn query(&self) -> PermissionState {
        self.backend.query("location")
    }

    /// Remaining `location` read allowance in this session (cooperative quota).
    pub fn reads_remaining(&self) -> u64 {
        self.quotas.remaining("location")
    }

    /// Read a position fix. Gated, in order: presence (`HardwareAbsent`) → consent
    /// (`ConsentDenied` while default-deny / not granted) → the `location` read quota
    /// (`PolicyBlocked` when exhausted). A successful read consumes ONE from the `location`
    /// bucket and NEVER touches the `egress` bucket (location-read ≠ location-send).
    pub fn read_fix(&self) -> Result<Fix, CapError> {
        // presence + consent in one canonical decision (HardwareAbsent / ConsentDenied / Ok).
        self.backend.acquire("location")?;
        if !self.quotas.try_consume("location", 1) {
            return Err(CapError::PolicyBlocked); // cooperative rate cap exhausted
        }
        // Honesty: no real GNSS — return a typed, accuracy-unknown placeholder.
        Ok(Fix { accuracy_m: f64::INFINITY, ..Fix::default() })
    }
}
