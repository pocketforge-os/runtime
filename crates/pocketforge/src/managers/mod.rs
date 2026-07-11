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
pub mod egress_log;
pub mod entropy;
pub mod input;
pub mod location;
pub mod sensors;
pub mod settings;
pub mod vibration;

pub use audio::{AudioManager, AudioSink};
pub use egress::{EgressAccounting, EgressManager, EgressReceipt};
pub use egress_log::{EgressEvent, EgressEventKind, EgressLog, EgressLogError};
pub use entropy::EntropyManager;
pub use input::InputManager;
pub use location::{Fix, LocationManager};
pub use sensors::SensorManager;
pub use settings::SettingsManager;
pub use vibration::VibrationManager;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
// Cooperative wall-clock token-bucket quota accounting (`tsp-ht0p.4`).
// ---------------------------------------------------------------------------
//
// The counter-per-capability that shipped through .3 (a `Mutex<HashMap<String,u64>>` that
// decremented to exhaustion and NEVER refilled — see the pre-`.4` history of this file) is
// upgraded IN PLACE to a wall-clock TOKEN BUCKET keyed on protection-tier defaults, per the
// bead work order + `docs/BROKER-DESIGN.md` §3 item 4. The public method surface is
// deliberately preserved (`new` / `remaining` / `set_remaining` / `try_consume`) so every
// merged consumer — `EnforcingBackend::with_quotas`, `LocationManager`, `EgressManager`, the
// broker.rs quota test, the STEP-2 transcript example — continues to compile and pass without
// weakening the load-bearing `buckets_are_independent` invariant.
//
// Tiers → default bucket config: `location`/`gnss` and per-op accounting for `egress` (all
// Dangerous per the .2 `Tier` enum) get finite (capacity, rate) pairs; `entropy` and every
// other Normal-tier capability get the [`UNGATED_BUCKET`] (infinite capacity + infinite rate)
// so `try_consume` is always `true` for them — a Normal cap is auto-grant-once-declared and
// never rate-limited (the enforce backend already short-circuits `entropy` before it reaches
// `try_consume`, but the ungated bucket makes the property structural rather than
// caller-dependent). The `.2` `Tier` enum lives in `pf-broker` (this crate cannot depend on it
// without a cycle), so the mapping is coded here as the tier-default lookup and cross-checked
// by tests — the coordinator boundary ruling (2026-07-11 21:27Z) that egress:<host>=Dangerous
// lands here as the `egress` bucket's finite-refill defaults.
//
// Fake-clock testability is the second bead deliverable: [`Clock`] is a trait, [`SystemClock`]
// is the default `Instant`-backed impl, and [`ManualClock`] is the shared, `advance()`-able
// clock STEP-3 uses. `Bucket::refill(now_ns)` is a pure function of the elapsed nanos, so
// tests never sleep.

/// The tier-default per-op token-bucket for `location` reads: 60 tokens burst, refilling at
/// one read per wall-clock second. The 60-burst matches the merged counter default (proven by
/// the retained `buckets_are_independent`); the 1/sec refill is a cooperative-tier rate cap —
/// the enforcement teeth are the seam design (see `docs/EGRESS-ENFORCEMENT-SEAM.md`).
pub const LOCATION_READ_QUOTA: u64 = 60;
/// The tier-default `location` refill rate (tokens per wall-clock second). Documented here so
/// the STEP-3 harness can advance the fake clock and prove the refill without magic numbers.
pub const LOCATION_READ_REFILL_PER_SEC: f64 = 1.0;

/// The tier-default per-op token-bucket for `egress` operations: 16-op burst; refills at one
/// op per four wall-clock seconds (≈ 15 ops/min). Deliberately tighter than reads because
/// exfiltration is the dangerous half. Byte accounting is a SEPARATE per-host tally (see
/// [`egress::EgressManager`]).
pub const EGRESS_QUOTA: u64 = 16;
/// The tier-default `egress` op-refill rate (tokens per wall-clock second).
pub const EGRESS_REFILL_PER_SEC: f64 = 0.25;

/// Static configuration for one bucket: `capacity` == max burst; `refill_per_sec` == steady-state
/// rate. `f64::INFINITY` in either field marks the ungated (Normal-tier) sentinel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BucketConfig {
    /// Maximum tokens the bucket can hold (burst).
    pub capacity: f64,
    /// Steady-state refill rate, tokens per wall-clock second.
    pub refill_per_sec: f64,
}

/// The Normal-tier / entropy sentinel: infinite capacity + infinite rate ⇒ every consume is
/// `true` regardless of clock. Documented so a reader sees the "auto-grant, never rate-limited"
/// property structurally.
pub const UNGATED_BUCKET: BucketConfig = BucketConfig {
    capacity: f64::INFINITY,
    refill_per_sec: f64::INFINITY,
};

impl BucketConfig {
    /// Is this the ungated sentinel? (Either infinite side qualifies — the intent is
    /// "structurally never limits".)
    pub fn is_ungated(&self) -> bool {
        self.capacity.is_infinite() || self.refill_per_sec.is_infinite()
    }
}

// --- clock seam ------------------------------------------------------------------------------

/// Monotonic wall-clock seam. Production uses [`SystemClock`] (`Instant`-backed); tests use
/// [`ManualClock`] so refill is a function of an EXPLICIT advance, never a `sleep`.
pub trait Clock: Send + Sync {
    /// Monotonic nanoseconds since the clock's epoch (implementation-defined; only DIFFERENCES
    /// are meaningful).
    fn now_ns(&self) -> u64;
}

/// The default production clock — `Instant::now()` since the clock was constructed.
#[derive(Debug)]
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    /// A fresh clock rooted at the current instant.
    pub fn new() -> SystemClock {
        SystemClock { start: Instant::now() }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        SystemClock::new()
    }
}

impl Clock for SystemClock {
    fn now_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }
}

/// A test clock that never moves on its own — [`advance`](Self::advance) is the only way its
/// `now_ns` changes. Cheap to share via `Arc`; every method takes `&self` so a test can advance
/// while the ledger holds an `Arc<dyn Clock>`.
#[derive(Debug, Default)]
pub struct ManualClock {
    ns: AtomicU64,
}

impl ManualClock {
    /// A fresh clock at t=0. Wrap in `Arc` to share between the ledger and the test driver.
    pub fn new() -> Arc<ManualClock> {
        Arc::new(ManualClock { ns: AtomicU64::new(0) })
    }

    /// Advance the clock by `d`. Idempotent under time (never rewinds).
    pub fn advance(&self, d: Duration) {
        self.ns.fetch_add(d.as_nanos() as u64, Ordering::Relaxed);
    }

    /// Convenience: advance by whole seconds.
    pub fn advance_secs(&self, s: u64) {
        self.advance(Duration::from_secs(s));
    }
}

impl Clock for ManualClock {
    fn now_ns(&self) -> u64 {
        self.ns.load(Ordering::Relaxed)
    }
}

// --- bucket state ----------------------------------------------------------------------------

/// The live token-bucket state for one capability.
#[derive(Debug)]
struct Bucket {
    cfg: BucketConfig,
    tokens: f64,
    last_ns: u64,
}

impl Bucket {
    fn new(cfg: BucketConfig, now_ns: u64) -> Bucket {
        Bucket { cfg, tokens: cfg.capacity, last_ns: now_ns }
    }

    fn refill(&mut self, now_ns: u64) {
        if self.cfg.is_ungated() {
            self.tokens = self.cfg.capacity;
            self.last_ns = now_ns;
            return;
        }
        let dt_ns = now_ns.saturating_sub(self.last_ns);
        if dt_ns == 0 {
            return;
        }
        let dt_s = dt_ns as f64 / 1_000_000_000.0;
        self.tokens = (self.tokens + dt_s * self.cfg.refill_per_sec).min(self.cfg.capacity);
        self.last_ns = now_ns;
    }

    fn try_consume(&mut self, n: f64) -> bool {
        if self.cfg.is_ungated() {
            return true;
        }
        if self.tokens + 1e-9 >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }

    fn remaining_int(&self) -> u64 {
        if self.cfg.is_ungated() {
            u64::MAX
        } else {
            self.tokens.max(0.0) as u64
        }
    }
}

/// A named cooperative wall-clock TOKEN-BUCKET ledger shared by every manager in one
/// [`Pf`](crate::Pf) session, so `location` (read) and `egress` (send) account into SEPARATE
/// buckets — consuming one never touches the other (the load-bearing accounting split the
/// epic calls out). Cooperative only (R-A): real enforcement teeth (netns + brokered nftables)
/// are the substrate-gated seam design at `docs/EGRESS-ENFORCEMENT-SEAM.md`.
pub struct QuotaLedger {
    clock: Arc<dyn Clock>,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl std::fmt::Debug for QuotaLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let buckets = self.buckets.lock().unwrap();
        f.debug_struct("QuotaLedger")
            .field("buckets", &buckets.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Default for QuotaLedger {
    fn default() -> Self {
        QuotaLedger::new()
    }
}

impl QuotaLedger {
    /// A fresh ledger backed by a [`SystemClock`]; buckets default lazily to their per-tier
    /// config on first touch. This is the shape every non-test caller uses.
    pub fn new() -> QuotaLedger {
        QuotaLedger::with_clock(Arc::new(SystemClock::new()))
    }

    /// A fresh ledger over an explicit [`Clock`] — the STEP-3 harness passes a [`ManualClock`]
    /// so refill is a deterministic function of `advance()`, no sleeps.
    pub fn with_clock(clock: Arc<dyn Clock>) -> QuotaLedger {
        QuotaLedger { clock, buckets: Mutex::new(HashMap::new()) }
    }

    /// The tier-default bucket config for `name`. Coded here (rather than sourced from
    /// `pf_broker::tier`) because this crate cannot depend on `pf-broker` without a cycle;
    /// broker.rs's tier tests are the cross-check that `location`/`gnss` and `egress`-op
    /// accounting stay in lockstep with the merged `Tier::Dangerous` set.
    pub fn default_config_for(name: &str) -> BucketConfig {
        match name.to_ascii_lowercase().as_str() {
            "location" | "gnss" => BucketConfig {
                capacity: LOCATION_READ_QUOTA as f64,
                refill_per_sec: LOCATION_READ_REFILL_PER_SEC,
            },
            "egress" => BucketConfig {
                capacity: EGRESS_QUOTA as f64,
                refill_per_sec: EGRESS_REFILL_PER_SEC,
            },
            // Every other capability — every Normal-tier cap, including entropy — is UNGATED
            // (`try_consume` always succeeds). Enforce.rs additionally short-circuits entropy
            // before reaching us, so this branch is defensive-in-depth.
            _ => UNGATED_BUCKET,
        }
    }

    /// Explicitly seed/override a bucket's remaining allowance (tests + policy). Preserves the
    /// tier-default capacity + refill rate; only the token float is set. `set_remaining(_, 0)`
    /// with the tier-default rate is exactly "no allowance until the clock refills."
    pub fn set_remaining(&self, name: &str, remaining: u64) {
        let cfg = Self::default_config_for(name);
        let now = self.clock.now_ns();
        let mut b = self.buckets.lock().unwrap();
        b.insert(
            name.to_string(),
            Bucket { cfg, tokens: remaining as f64, last_ns: now },
        );
    }

    /// Explicitly install a bucket with a custom [`BucketConfig`] (per-host byte quotas +
    /// policy overrides). Preserves the current-clock alignment.
    pub fn install_bucket(&self, name: &str, cfg: BucketConfig, tokens: u64) {
        let now = self.clock.now_ns();
        let mut b = self.buckets.lock().unwrap();
        b.insert(
            name.to_string(),
            Bucket { cfg, tokens: tokens as f64, last_ns: now },
        );
    }

    /// Remaining allowance for `name` (lazily defaulted, refill-current). Returns `u64::MAX`
    /// for an ungated bucket — cheap "is there room?" checks stay branchless at call sites.
    pub fn remaining(&self, name: &str) -> u64 {
        let now = self.clock.now_ns();
        let mut b = self.buckets.lock().unwrap();
        let bucket = b
            .entry(name.to_string())
            .or_insert_with(|| Bucket::new(Self::default_config_for(name), now));
        bucket.refill(now);
        bucket.remaining_int()
    }

    /// Try to consume `n` from `name`'s bucket. Refills first (wall-clock elapsed × refill
    /// rate, capped at capacity) then decrements; returns `true` if it fit, `false` if the
    /// bucket is throttling. Touching `name` NEVER touches any other bucket. An ungated bucket
    /// always returns `true` (Normal-tier / entropy contract).
    pub fn try_consume(&self, name: &str, n: u64) -> bool {
        let now = self.clock.now_ns();
        let mut b = self.buckets.lock().unwrap();
        let bucket = b
            .entry(name.to_string())
            .or_insert_with(|| Bucket::new(Self::default_config_for(name), now));
        bucket.refill(now);
        bucket.try_consume(n as f64)
    }

    /// The active bucket config for `name` — read-only, useful for the STEP-3 harness + the
    /// `pf-permissions` inspection surface.
    pub fn config_of(&self, name: &str) -> BucketConfig {
        let now = self.clock.now_ns();
        let mut b = self.buckets.lock().unwrap();
        let bucket = b
            .entry(name.to_string())
            .or_insert_with(|| Bucket::new(Self::default_config_for(name), now));
        bucket.cfg
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
    fn entropy_and_normal_tier_are_ungated_buckets() {
        // The Normal-tier / entropy contract: `try_consume` NEVER refuses regardless of `n` or
        // wall-clock. Assert both the direct sentinel + a huge-`n` consume from a fresh ledger.
        let q = QuotaLedger::new();
        assert!(QuotaLedger::default_config_for("entropy").is_ungated());
        assert!(QuotaLedger::default_config_for("vibration").is_ungated());
        assert!(QuotaLedger::default_config_for("audio").is_ungated());
        assert!(q.try_consume("entropy", 1_000_000_000));
        assert!(q.try_consume("vibration", 1_000_000_000));
        assert_eq!(q.remaining("entropy"), u64::MAX);
    }

    #[test]
    fn wall_clock_refill_returns_tokens_over_time() {
        // The full round trip STEP-3 asserts under the enforce path: exhaust, advance the
        // MANUAL clock, refill, consume succeeds. Uses `ManualClock` so no `sleep` — the
        // refill is a pure function of `now_ns - last_ns`.
        let clock = ManualClock::new();
        let q = QuotaLedger::with_clock(clock.clone());
        q.set_remaining("location", 0);
        assert!(!q.try_consume("location", 1), "empty bucket refuses");
        // 5 seconds at 1/sec = 5 tokens (capped at capacity 60).
        clock.advance_secs(5);
        assert_eq!(q.remaining("location"), 5, "refill = elapsed × rate");
        assert!(q.try_consume("location", 5));
        assert!(!q.try_consume("location", 1), "drained again after refill-consume");
    }

    #[test]
    fn refill_saturates_at_capacity() {
        let clock = ManualClock::new();
        let q = QuotaLedger::with_clock(clock.clone());
        q.set_remaining("egress", 0);
        // Wait ridiculously long: refill saturates at the 16-op burst.
        clock.advance_secs(10_000);
        assert_eq!(q.remaining("egress"), EGRESS_QUOTA, "refill saturates at capacity");
    }

    #[test]
    fn refill_after_partial_consume_is_incremental() {
        // Refill accrues off the LAST update, not the last consume — partial-refills stack.
        let clock = ManualClock::new();
        let q = QuotaLedger::with_clock(clock.clone());
        q.set_remaining("location", 10);
        assert!(q.try_consume("location", 8)); // 2 left
        assert_eq!(q.remaining("location"), 2);
        clock.advance_secs(3); // +3 tokens at 1/sec = 5
        assert_eq!(q.remaining("location"), 5, "incremental refill accrues");
        clock.advance_secs(60); // saturate
        assert_eq!(q.remaining("location"), LOCATION_READ_QUOTA);
    }

    #[test]
    fn tier_defaults_match_the_merged_dangerous_set() {
        // The .2 Tier enum classifies location/gnss + specific-host egress as Dangerous, and
        // everything else Normal (ungated). Cross-check the tier-default lookup here — this is
        // the seam that keeps the pocketforge crate + pf_broker::tier crate in lockstep
        // without a dep cycle.
        assert!(!QuotaLedger::default_config_for("location").is_ungated());
        assert!(!QuotaLedger::default_config_for("gnss").is_ungated());
        assert!(!QuotaLedger::default_config_for("egress").is_ungated());
        // Every Normal-tier known cap is ungated.
        for c in ["input", "vibration", "rumble", "leds", "audio", "settings", "entropy",
                  "imu", "accelerometer", "gyroscope", "magnetometer"] {
            assert!(QuotaLedger::default_config_for(c).is_ungated(), "{c} should be ungated");
        }
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
