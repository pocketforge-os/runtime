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
            Violation::Malformed(t) => write!(f, "malformed use entry '{t}'"),
        }
    }
}

/// `egress` is a platform network-send capability (no descriptor hardware row) — declarable as
/// `egress:<host>`. It is NOT in `KNOWN_CAPS` (which is hardware/platform device caps), so the
/// validator knows about it explicitly.
pub const EGRESS_CAP: &str = "egress";

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

    /// Validate the `use = [...]` graph against the device descriptor (the CEILING check). Returns
    /// the [`ValidatedManifest`] or the full list of [`Violation`]s (all of them, not just the first).
    pub fn validate(&self, descriptor: &Descriptor) -> Result<ValidatedManifest, Vec<Violation>> {
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
