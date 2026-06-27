//! Per-capability **managers** (`tsp-e1b.4`) — one device-agnostic object per capability,
//! each multiplexing apps onto one device behind the `.2` facade. Every manager wraps the
//! SAME [`Backend`](crate::backend::Backend) trait the facade already uses, so the in-process
//! ↔ broker **backend swap** holds for the manager layer too (the load-bearing "survives the
//! runtime fork" property): structural reads come from the descriptor the client holds; access,
//! permission, and actuation route through the backend.
//!
//! ## Descriptor = expectation, probe = ground truth
//!
//! Each manager checks capability presence as `descriptor ∧ ¬probe-says-absent` — the
//! [`HardwareProbe`] seam ([`reconcile_presence`]). Off-hardware (no evdev/IIO node) the probe
//! returns `None` ⇒ the descriptor is trusted; on silicon a [`LiveProbe`] reads
//! `EVIOCGBIT`/`EVIOCGABS`/IIO and can DEMOTE a descriptor-advertised cap to `HardwareAbsent`
//! (the "DT-but-unbound" hazard) — never a crash, never a fabricated row. The authoritative
//! on-silicon reconciliation is HARDWARE-GATED (owner return + explicit OK).
//!
//! ## Honesty (R-A)
//!
//! These managers are the **cooperative v0 contract** — they prove the capability shape,
//! ergonomics, descriptor-honest missing-hardware degradation, the E4 accessibility
//! enforcement point, and the location-read ≠ location-send accounting split. They are NOT
//! enforcement: real default-deny-vs-hostile, unforgeable handles, and server-side quotas are
//! the out-of-process broker (`.3`) on the Phase-2 substrate. INPUT is the one v0-enforceable
//! cap (`uinput`+`EVIOCGRAB`, `.6`).

pub mod audio;
pub mod egress;
pub mod entropy;
pub mod input;
pub mod location;
pub mod sensors;
pub mod settings;
pub mod vibration;

pub use audio::{AudioManager, AudioSink};
pub use egress::{EgressManager, EgressReceipt};
pub use entropy::EntropyManager;
pub use input::InputManager;
pub use location::{Fix, LocationManager};
pub use sensors::SensorManager;
pub use settings::SettingsManager;
pub use vibration::VibrationManager;

use std::collections::HashMap;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// The live-probe reconciliation seam.
// ---------------------------------------------------------------------------

/// The ground-truth hardware probe (`EVIOCGBIT`/`EVIOCGABS`/IIO sysfs). It only ever *demotes*
/// a descriptor claim — it never invents a capability the descriptor omitted.
pub trait HardwareProbe: Send + Sync {
    /// Ground-truth presence for a capability `name`. `Some(true)` = the live probe confirms it;
    /// `Some(false)` = the probe says the hardware is absent **despite** the descriptor (demote
    /// to `HardwareAbsent`); `None` = cannot probe here (off-hardware / no node) ⇒ trust the
    /// descriptor.
    fn probe_present(&self, name: &str) -> Option<bool>;
}

/// The off-hardware default: every probe is inconclusive (`None`) ⇒ the descriptor is trusted.
/// This is what the simulator + CI legs run with; [`LiveProbe`] replaces it on silicon.
pub struct DescriptorTrustProbe;

impl HardwareProbe for DescriptorTrustProbe {
    fn probe_present(&self, _name: &str) -> Option<bool> {
        None
    }
}

/// A best-effort on-host probe. Today it only checks IIO sysfs presence (harmless off-hardware:
/// the path does not exist ⇒ `None` ⇒ trust the descriptor). The full `EVIOCGBIT`/`EVIOCGABS`
/// reconciliation against the gamepad evdev node is HARDWARE-GATED (SPIKE-0 / `tsp-9sx.1`); this
/// type is the seam that work slots into, not the finished probe.
pub struct LiveProbe {
    /// IIO sysfs root (`/sys/bus/iio/devices`, or `PF_IIO_ROOT` under the sim).
    pub iio_root: std::path::PathBuf,
}

impl LiveProbe {
    /// Build a probe rooted at the standard IIO sysfs path (or `PF_IIO_ROOT` if set, as the sim
    /// binds it).
    pub fn new() -> LiveProbe {
        let root = std::env::var_os("PF_IIO_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/sys/bus/iio/devices"));
        LiveProbe { iio_root: root }
    }
}

impl Default for LiveProbe {
    fn default() -> Self {
        LiveProbe::new()
    }
}

impl HardwareProbe for LiveProbe {
    fn probe_present(&self, name: &str) -> Option<bool> {
        // Only inertial caps have an IIO node to check; everything else is inconclusive here.
        let inertial = matches!(
            name.to_ascii_lowercase().as_str(),
            "imu" | "accelerometer" | "gyroscope" | "magnetometer"
        );
        if !inertial {
            return None;
        }
        // No IIO root at all ⇒ we are off-hardware; stay inconclusive (trust the descriptor).
        let entries = std::fs::read_dir(&self.iio_root).ok()?;
        // Any `iio:deviceN` present ⇒ confirm; an existing-but-empty root ⇒ demote.
        let any = entries.flatten().any(|e| {
            e.file_name().to_string_lossy().starts_with("iio:device")
        });
        Some(any)
    }
}

/// Reconcile a descriptor claim against the probe: present **iff** the descriptor advertises it
/// AND the live probe does not contradict it. This is the one rule every manager uses.
pub fn reconcile_presence(descriptor_present: bool, probe: &dyn HardwareProbe, name: &str) -> bool {
    descriptor_present && probe.probe_present(name) != Some(false)
}

// ---------------------------------------------------------------------------
// Cooperative quota accounting (location-read ≠ location-send).
// ---------------------------------------------------------------------------

/// Default per-session quota for `location` reads (a cooperative rate cap; the real token-bucket
/// is the `.3` broker / E3 policy).
pub const LOCATION_READ_QUOTA: u64 = 60;
/// Default per-session quota for `egress` (network *send*) operations — deliberately tighter than
/// reads, because exfiltration is the dangerous half (location-read ≠ location-send).
pub const EGRESS_QUOTA: u64 = 16;

/// A named cooperative quota ledger shared by every manager in one [`Pf`](crate::Pf) session, so
/// `location` (read) and `egress` (send) account into SEPARATE buckets — consuming one never
/// touches the other (the accounting the epic calls out). Cooperative only (R-A): the enforcing
/// token-bucket is the `.3` broker.
#[derive(Debug, Default)]
pub struct QuotaLedger {
    buckets: Mutex<HashMap<String, u64>>,
}

impl QuotaLedger {
    /// A fresh ledger; buckets default lazily to their per-capability quota on first touch.
    pub fn new() -> QuotaLedger {
        QuotaLedger { buckets: Mutex::new(HashMap::new()) }
    }

    fn default_for(name: &str) -> u64 {
        match name {
            "location" => LOCATION_READ_QUOTA,
            "egress" => EGRESS_QUOTA,
            _ => u64::MAX, // unknown bucket ⇒ ungated (cooperative)
        }
    }

    /// Explicitly seed/override a bucket's remaining allowance (tests + policy).
    pub fn set_remaining(&self, name: &str, remaining: u64) {
        self.buckets.lock().unwrap().insert(name.to_string(), remaining);
    }

    /// Remaining allowance for `name` (lazily defaulted).
    pub fn remaining(&self, name: &str) -> u64 {
        let mut b = self.buckets.lock().unwrap();
        *b.entry(name.to_string()).or_insert_with(|| Self::default_for(name))
    }

    /// Try to consume `n` from `name`'s bucket. Returns `true` if it fit (and decremented),
    /// `false` if the bucket would underflow (the cooperative "quota exhausted" signal). Touching
    /// `name` NEVER touches any other bucket.
    pub fn try_consume(&self, name: &str, n: u64) -> bool {
        let mut b = self.buckets.lock().unwrap();
        let slot = b.entry(name.to_string()).or_insert_with(|| Self::default_for(name));
        if *slot >= n {
            *slot -= n;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_independent() {
        let q = QuotaLedger::new();
        let loc0 = q.remaining("location");
        assert_eq!(loc0, LOCATION_READ_QUOTA);
        // Consuming egress must not touch the location bucket.
        assert!(q.try_consume("egress", 3));
        assert_eq!(q.remaining("egress"), EGRESS_QUOTA - 3);
        assert_eq!(q.remaining("location"), loc0, "egress consumption leaked into location");
    }

    #[test]
    fn quota_exhaustion_is_a_clean_false() {
        let q = QuotaLedger::new();
        q.set_remaining("egress", 2);
        assert!(q.try_consume("egress", 2));
        assert!(!q.try_consume("egress", 1), "exhausted bucket refuses, does not underflow");
        assert_eq!(q.remaining("egress"), 0);
    }

    #[test]
    fn reconcile_demotes_only_on_explicit_absent() {
        struct Yes;
        struct No;
        struct Dunno;
        impl HardwareProbe for Yes {
            fn probe_present(&self, _: &str) -> Option<bool> {
                Some(true)
            }
        }
        impl HardwareProbe for No {
            fn probe_present(&self, _: &str) -> Option<bool> {
                Some(false)
            }
        }
        impl HardwareProbe for Dunno {
            fn probe_present(&self, _: &str) -> Option<bool> {
                None
            }
        }
        // Descriptor says present:
        assert!(reconcile_presence(true, &Yes, "imu"));
        assert!(reconcile_presence(true, &Dunno, "imu"), "inconclusive ⇒ trust descriptor");
        assert!(!reconcile_presence(true, &No, "imu"), "probe demotes DT-but-unbound");
        // Descriptor says absent ⇒ never fabricated, whatever the probe claims:
        assert!(!reconcile_presence(false, &Yes, "imu"));
    }
}
