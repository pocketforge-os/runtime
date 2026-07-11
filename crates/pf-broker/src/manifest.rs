//! The **`app.toml` launch contract** — the CapDL-style **static authority graph** the broker
//! validates before an app runs. `use = [...]` is the CEILING of what the app may ever acquire;
//! the broker refuses anything outside it at runtime (the manifest, not the app, bounds authority).
//!
//! Validation at launch REJECTS:
//!   * **unknown** capabilities (not a platform capability at all);
//!   * **duplicate** entries (a malformed/over-specified graph);
//!   * **bad modifiers** (e.g. `location:teleport`);
//!   * **undescriptored REQUIRED** capabilities — a *required* hardware cap this device's E1
//!     descriptor cannot back (the over-broad/dangling route).
//!
//! It ACCEPTS a *required* platform cap (input/entropy/audio/settings — always backable),
//! `egress:<host>` (a platform network capability, no descriptor row), and any `cap?`
//! (OPTIONAL) entry even when the descriptor can't back it — the app handles the runtime
//! `HardwareAbsent` (the graceful cross-device degradation `.4` proved). The `?`/modifier
//! vocabulary is v0 and **co-designed with E3** (`infra-102`) when it is filed.

use std::collections::BTreeSet;

use pocketforge::backend::is_known;
use pocketforge::Descriptor;
use serde::Deserialize;

use crate::tier::{tier_of, Tier};

/// One parsed `use = [...]` entry: a base capability, an optional scope modifier, and whether it
/// is OPTIONAL (`cap?` ⇒ graceful-degradation allowed if the device can't back it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseEntry {
    pub cap: String,
    pub modifier: Option<String>,
    pub optional: bool,
    /// The original token (for diagnostics).
    pub raw: String,
}

impl UseEntry {
    /// Parse one `use` token: `"<cap>[:<modifier>][?]"`.
    pub fn parse(token: &str) -> UseEntry {
        let raw = token.to_string();
        let t = token.trim();
        let (t, optional) = match t.strip_suffix('?') {
            Some(stripped) => (stripped, true),
            None => (t, false),
        };
        let (cap, modifier) = match t.split_once(':') {
            Some((c, m)) => (c.trim().to_ascii_lowercase(), Some(m.trim().to_string())),
            None => (t.trim().to_ascii_lowercase(), None),
        };
        UseEntry { cap, modifier, optional, raw }
    }
}

/// A reason an `app.toml` was rejected at launch (typed, so the supervisor logs *why*).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// Not a capability the platform knows about at all.
    UnknownCapability(String),
    /// The same capability declared more than once.
    DuplicateCapability(String),
    /// A modifier the capability does not accept (or a malformed one).
    BadModifier { cap: String, modifier: String },
    /// A REQUIRED hardware capability this device's descriptor cannot back (over-broad route).
    UndescriptoredRequired(String),
    /// A **signature-tier** capability (raw/arbitrary egress, or a future first-party-only cap) was
    /// declared by an app that is neither first-party-signed nor covered by a blessed-binary
    /// exemption. The launch is REJECTED (R-C / LOCKED #3 signature tier). Carries the offending
    /// `use=[]` token (e.g. `"egress:0.0.0.0/0"`).
    SignatureTierRequiresTrust { token: String },
    /// A structurally malformed entry (empty cap, etc.).
    Malformed(String),
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Violation::UnknownCapability(c) => write!(f, "unknown capability '{c}'"),
            Violation::DuplicateCapability(c) => write!(f, "duplicate capability '{c}'"),
            Violation::BadModifier { cap, modifier } => {
                write!(f, "capability '{cap}' rejects modifier '{modifier}'")
            }
            Violation::UndescriptoredRequired(c) => {
                write!(f, "required capability '{c}' is not backed by this device's descriptor (mark it '{c}?' to allow graceful absence)")
            }
            Violation::SignatureTierRequiresTrust { token } => {
                write!(f, "signature-tier capability '{token}' requires a first-party or blessed-binary signature")
            }
            Violation::Malformed(t) => write!(f, "malformed use entry '{t}'"),
        }
    }
}

/// `egress` is a platform network-send capability (no descriptor hardware row) — declarable as
/// `egress:<host>`. It is NOT in `KNOWN_CAPS` (which is hardware/platform device caps), so the
/// validator knows about it explicitly.
pub const EGRESS_CAP: &str = "egress";

/// An enumerated, first-party-signed **blessed-binary** broad-grant (R-C).
///
/// A blessed binary is a closed app that cannot be transparently brokered (e.g. Steam Link — it is
/// both a uinput *consumer* and *producer*, so the `EVIOCGRAB` interposition would break it). Rather
/// than force it through the broker, the platform grants it an *enumerated, signed* broad authority
/// and confines it by cgroup/namespace/seccomp only (NOT capability-brokering). This registration is
/// the validator's **canonical exemption case**: it clears [`Tier::Signature`] entries that the app
/// could not otherwise declare, but ONLY for the enumerated `app_id` and only the enumerated
/// `grants`. The registration ITSELF is first-party-signed and keyed to the signed-fetch `sha256`
/// the supervisor verified — trust = first-party enrollment + signed-fetch SHA. See
/// `docs/PERMISSION-MODEL.md` "Blessed-binary exemption" for the threat-model caveat.
#[derive(Debug, Clone)]
pub struct BlessedRegistration {
    /// The app id this exemption is enrolled for (must match the manifest's `[app] id`).
    pub app_id: String,
    /// The signed-fetch SHA-256 the supervisor verified the bundle against (provenance anchor).
    pub sha256: String,
    /// The broad grants this blessed binary is enumerated to hold, as `use=[]` tokens
    /// (e.g. `"egress:0.0.0.0/0"`) or bare cap names (a whole-capability grant).
    pub grants: Vec<String>,
}

impl BlessedRegistration {
    /// Does this registration enumerate `entry` (matching the full token or the bare capability)?
    pub fn covers(&self, entry: &UseEntry) -> bool {
        let token = entry.raw.trim().trim_end_matches('?').trim().to_ascii_lowercase();
        self.grants.iter().any(|g| {
            let g = g.trim().to_ascii_lowercase();
            g == token || g == entry.cap
        })
    }
}

/// The **trust class of an app bundle at launch** — the INPUT that decides whether a
/// [`Tier::Signature`] request is permitted.
///
/// It is established by the supervisor *before* manifest validation and passed in: an `app.toml.sig`
/// minisign verification against the first-party `release.d` key directory sets [`first_party`], and
/// a blessed-binary registry lookup sets [`blessed`]; an app that clears neither is untrusted. **`.2`
/// does NOT implement that signature verification** — this type is only the seam the existing
/// two-signature machinery + `.5`'s trust-chain design feed. The default ([`LaunchTrust::UNTRUSTED`],
/// used by [`AppManifest::validate`]) is the safe floor: no signature-tier authority.
///
/// [`first_party`]: LaunchTrust::first_party
/// [`blessed`]: LaunchTrust::blessed
#[derive(Debug, Clone, Copy)]
pub struct LaunchTrust<'a> {
    /// The bundle is signed by the first-party release key (`app.toml.sig` verified) — clears ALL
    /// signature-tier entries.
    pub first_party: bool,
    /// A blessed-binary registration covering this app, if any — clears the enumerated signature-tier
    /// entries for the matching `app_id` only.
    pub blessed: Option<&'a BlessedRegistration>,
}

impl LaunchTrust<'_> {
    /// The safe floor: no signing authority (used by the back-compat [`AppManifest::validate`]).
    pub const UNTRUSTED: LaunchTrust<'static> = LaunchTrust { first_party: false, blessed: None };
    /// A first-party-signed bundle (clears every signature-tier entry).
    pub const FIRST_PARTY: LaunchTrust<'static> = LaunchTrust { first_party: true, blessed: None };

    /// A blessed-binary trust context over `reg` (not first-party, but exempt for what `reg` enumerates).
    pub fn blessed(reg: &BlessedRegistration) -> LaunchTrust<'_> {
        LaunchTrust { first_party: false, blessed: Some(reg) }
    }

    /// Does this trust context authorize declaring the signature-tier `entry` for `app_id`?
    fn authorizes_signature(&self, app_id: &str, entry: &UseEntry) -> bool {
        if self.first_party {
            return true;
        }
        self.blessed
            .is_some_and(|reg| reg.app_id.eq_ignore_ascii_case(app_id) && reg.covers(entry))
    }
}

fn modifier_ok(cap: &str, modifier: &str) -> bool {
    match cap {
        // Location scope is the privacy fuzzing tier (Android-style); E3 owns the full policy.
        "location" | "gnss" => matches!(modifier, "approximate" | "precise"),
        // Egress modifier is the destination host (any non-empty token).
        EGRESS_CAP => !modifier.trim().is_empty(),
        // No other capability takes a modifier in v0.
        _ => false,
    }
}

/// A descriptor-validated manifest: the parsed entries plus the resolved **allowed cap set**
/// (the runtime ceiling) and the declared egress hosts.
#[derive(Debug, Clone)]
pub struct ValidatedManifest {
    pub app_id: String,
    pub entries: Vec<UseEntry>,
    allowed: BTreeSet<String>,
    egress_hosts: BTreeSet<String>,
}

impl ValidatedManifest {
    /// Is `cap` within the manifest ceiling (declared)?
    pub fn allows(&self, cap: &str) -> bool {
        self.allowed.contains(&cap.to_ascii_lowercase())
    }

    /// The declared egress destination hosts.
    pub fn egress_hosts(&self) -> impl Iterator<Item = &str> {
        self.egress_hosts.iter().map(String::as_str)
    }

    /// The full declared ceiling (base capability names).
    pub fn allowed_caps(&self) -> impl Iterator<Item = &str> {
        self.allowed.iter().map(String::as_str)
    }
}

/// A raw `app.toml`: `[app] id, use`.
#[derive(Debug, Deserialize)]
pub struct AppManifest {
    pub app: AppSection,
}

/// The `[app]` table.
#[derive(Debug, Deserialize)]
pub struct AppSection {
    pub id: String,
    #[serde(default, rename = "use")]
    pub uses: Vec<String>,
}

impl AppManifest {
    /// Parse an `app.toml` from a string.
    pub fn from_toml(s: &str) -> Result<AppManifest, toml::de::Error> {
        toml::from_str(s)
    }

    /// Load an `app.toml` from a path.
    pub fn load(path: impl AsRef<std::path::Path>) -> std::io::Result<AppManifest> {
        let text = std::fs::read_to_string(path)?;
        AppManifest::from_toml(&text).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
    }

    /// Validate the `use = [...]` graph against the device descriptor (the CEILING check), treating
    /// the app as **untrusted** ([`LaunchTrust::UNTRUSTED`]) — the safe back-compat default. Returns
    /// the [`ValidatedManifest`] or the full list of [`Violation`]s (all of them, not just the first).
    ///
    /// An untrusted app may declare Normal + Dangerous capabilities but NOT [`Tier::Signature`] ones
    /// (raw/arbitrary egress): use [`validate_with_trust`](Self::validate_with_trust) to supply the
    /// first-party / blessed-binary trust that clears the signature tier.
    pub fn validate(&self, descriptor: &Descriptor) -> Result<ValidatedManifest, Vec<Violation>> {
        self.validate_with_trust(descriptor, &LaunchTrust::UNTRUSTED)
    }

    /// Validate the `use = [...]` graph against the device descriptor AND the app's launch
    /// [`LaunchTrust`]. Identical to [`validate`](Self::validate) plus the **signature-tier gate**: a
    /// [`Tier::Signature`] capability (raw/arbitrary egress, or a future first-party-only cap) is a
    /// [`Violation::SignatureTierRequiresTrust`] unless `trust` is first-party or a blessed-binary
    /// registration enumerates it. `trust` is an INPUT the supervisor computes ahead of validation
    /// (`app.toml.sig` verify → first-party; blessed registry lookup → blessed); `.2` does not verify
    /// signatures itself.
    pub fn validate_with_trust(
        &self,
        descriptor: &Descriptor,
        trust: &LaunchTrust,
    ) -> Result<ValidatedManifest, Vec<Violation>> {
        let mut violations = Vec::new();
        let mut allowed = BTreeSet::new();
        let mut egress_hosts = BTreeSet::new();
        let mut entries = Vec::new();
        let mut seen = BTreeSet::new();

        for token in &self.uses_iter() {
            let e = UseEntry::parse(token);
            if e.cap.is_empty() {
                violations.push(Violation::Malformed(e.raw.clone()));
                continue;
            }
            if !seen.insert(e.cap.clone()) {
                violations.push(Violation::DuplicateCapability(e.cap.clone()));
                continue;
            }
            // Modifier rules.
            if let Some(m) = &e.modifier {
                if !modifier_ok(&e.cap, m) {
                    violations.push(Violation::BadModifier { cap: e.cap.clone(), modifier: m.clone() });
                    continue;
                }
            }
            // Signature-tier gate (R-C): a first-party-only capability (raw/arbitrary egress, …) may
            // only be declared by a first-party-signed or blessed-binary-enumerated app. This runs
            // before the egress/known/descriptor checks so an unauthorized signature request is
            // rejected for the RIGHT reason (not as an over-broad-route or unknown-cap).
            if tier_of(&e.cap, e.modifier.as_deref()) == Tier::Signature
                && !trust.authorizes_signature(&self.app.id, &e)
            {
                violations.push(Violation::SignatureTierRequiresTrust { token: e.raw.clone() });
                continue;
            }
            if e.cap == EGRESS_CAP {
                if let Some(host) = &e.modifier {
                    egress_hosts.insert(host.clone());
                }
                allowed.insert(e.cap.clone());
                entries.push(e);
                continue;
            }
            if !is_known(&e.cap) {
                violations.push(Violation::UnknownCapability(e.cap.clone()));
                continue;
            }
            // A REQUIRED hardware cap the descriptor cannot back is an over-broad route. (Platform
            // caps input/entropy/audio/settings are always present; optional `cap?` is allowed to
            // be absent — the app degrades to HardwareAbsent at runtime.)
            if !e.optional && !descriptor.cap_present(&e.cap) {
                violations.push(Violation::UndescriptoredRequired(e.cap.clone()));
                continue;
            }
            allowed.insert(e.cap.clone());
            entries.push(e);
        }

        if violations.is_empty() {
            Ok(ValidatedManifest { app_id: self.app.id.clone(), entries, allowed, egress_hosts })
        } else {
            Err(violations)
        }
    }

    fn uses_iter(&self) -> Vec<String> {
        self.app.uses.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(id: &str) -> Descriptor {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pocketforge/tests/fixtures")
            .join(format!("{id}-capabilities.toml"));
        Descriptor::load(p).expect("fixture")
    }

    fn manifest(uses: &[&str]) -> AppManifest {
        AppManifest {
            app: AppSection { id: "com.test.app".into(), uses: uses.iter().map(|s| s.to_string()).collect() },
        }
    }

    #[test]
    fn parse_handles_modifier_and_optional() {
        assert_eq!(UseEntry::parse("vibration"), UseEntry { cap: "vibration".into(), modifier: None, optional: false, raw: "vibration".into() });
        let e = UseEntry::parse("location:approximate");
        assert_eq!((e.cap.as_str(), e.modifier.as_deref(), e.optional), ("location", Some("approximate"), false));
        let o = UseEntry::parse("imu?");
        assert_eq!((o.cap.as_str(), o.optional), ("imu", true));
        let eg = UseEntry::parse("egress:steampowered.com");
        assert_eq!((eg.cap.as_str(), eg.modifier.as_deref()), ("egress", Some("steampowered.com")));
    }

    #[test]
    fn a523_well_formed_manifest_validates() {
        let m = manifest(&["input", "vibration", "imu", "entropy", "location:approximate?", "egress:steampowered.com"]);
        let v = m.validate(&desc("a523")).expect("a523 backs imu + rumble");
        assert!(v.allows("imu") && v.allows("vibration") && v.allows("egress"));
        assert_eq!(v.egress_hosts().collect::<Vec<_>>(), ["steampowered.com"]);
    }

    #[test]
    fn required_imu_on_a133_is_rejected_but_optional_is_allowed() {
        // a133 has no IMU. Required → reject (over-broad); optional → accept (graceful absence).
        let req = manifest(&["imu"]).validate(&desc("a133"));
        assert_eq!(req.unwrap_err(), vec![Violation::UndescriptoredRequired("imu".into())]);
        let opt = manifest(&["imu?"]).validate(&desc("a133")).expect("optional imu allowed");
        assert!(opt.allows("imu"), "optional imu is in the ceiling (runtime returns HardwareAbsent)");
    }

    #[test]
    fn unknown_dup_and_bad_modifier_are_each_rejected() {
        let v = manifest(&["telepathy", "input", "input", "location:teleport"]).validate(&desc("a523")).unwrap_err();
        assert!(v.contains(&Violation::UnknownCapability("telepathy".into())));
        assert!(v.contains(&Violation::DuplicateCapability("input".into())));
        assert!(v.contains(&Violation::BadModifier { cap: "location".into(), modifier: "teleport".into() }));
    }

    #[test]
    fn platform_caps_need_no_descriptor_row() {
        // input/entropy/audio/settings are always backable, even required.
        let v = manifest(&["input", "entropy", "audio", "settings"]).validate(&desc("a133")).expect("platform caps ok");
        for c in ["input", "entropy", "audio", "settings"] {
            assert!(v.allows(c));
        }
    }
}
