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
    /// The app pins an `[runtime] abi` this Platform does not offer (the frozen-contract version
    /// the running Platform advertises does not include the app's pinned `abi`).
    UnsupportedAbi { abi: String },
    /// The app pins an `[runtime] family` that is neither THIS Platform's canonical family id nor
    /// one of its accepted aliases — a build for a different SoC family (different kernel/GPU/SDL).
    FamilyMismatch { app_family: String, platform_family: String },
    /// [`AppManifest::check_runtime`] was asked to verify a launch that requires a Platform pin, but
    /// the `app.toml` carries no `[runtime]` table (parsing stays back-compatible; requiring the pin
    /// is the supervisor's launch-policy choice).
    MissingRuntime,
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
            Violation::UnsupportedAbi { abi } => {
                write!(f, "this Platform does not offer ABI '{abi}'")
            }
            Violation::FamilyMismatch { app_family, platform_family } => write!(
                f,
                "app targets family '{app_family}' but this Platform is '{platform_family}' (nor an accepted alias)"
            ),
            Violation::MissingRuntime => {
                write!(f, "app.toml has no [runtime] family/abi pin (required for this launch)")
            }
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

/// A raw `app.toml`: `[app] id, use` plus the optional `[runtime]` Platform pin.
#[derive(Debug, Deserialize)]
pub struct AppManifest {
    pub app: AppSection,
    /// The `[runtime]` Platform pin (family + frozen-ABI version). **Optional** so an `app.toml`
    /// written before the pin existed still parses (back-compat); a launch that *requires* the pin
    /// gets [`Violation::MissingRuntime`] from [`check_runtime`](Self::check_runtime).
    #[serde(default)]
    pub runtime: Option<RuntimeSection>,
}

/// The `[app]` table.
#[derive(Debug, Deserialize)]
pub struct AppSection {
    pub id: String,
    #[serde(default, rename = "use")]
    pub uses: Vec<String>,
}

/// The `[runtime]` table — an app's **Platform pin**: the per-SoC *family* it was built for and the
/// frozen `libpocketforge`/PFW1 `abi` version it links. Reconciled from
/// `runtime/docs/RUNTIME-SDK-SPLIT.md` §2 + the canonical registry `platform/abi/families.toml`.
///
/// The **static** package-time validator (`platform/core/appmanifest.py`, `tsp-ziac.1`) already
/// rejects unknown-family / out-of-lock-version at packaging; this on-device table is the
/// **cooperative** launch-time consumer — [`check_runtime`](AppManifest::check_runtime) rejects an
/// app whose `family`/`abi` this running Platform does not offer (the "one descriptor, three
/// consumers" third consumer). `platform-version` (the frozen substrate SHA-set pin) is parsed but
/// NOT matched here — out-of-lock-version is the static validator's job, and the on-device broker
/// holds no lock (`platform.lock` lives in the `platform` repo).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RuntimeSection {
    /// The canonical per-SoC family id this build targets (e.g. `pocketforge/a133-powervr`), or an
    /// accepted alias (`pocketforge/sun50i-a133`).
    pub family: String,
    /// The frozen `libpocketforge`/PFW1 contract version this build links (e.g. `"1"`).
    pub abi: String,
    /// The frozen substrate SHA-set this build pins (E8). Parsed for completeness; version-in-lock
    /// checking is the static package-time validator's job, not the broker's.
    #[serde(default, rename = "platform-version")]
    pub platform_version: Option<String>,
}

/// **What family am I?** — the running Platform's own family advertisement, the INPUT
/// [`check_runtime`](AppManifest::check_runtime) matches an app's `[runtime]` pin against.
///
/// The supervisor supplies this; it is **sourced from the device/platform config, NOT hardcoded in
/// Rust** (hardcoding a family registry here would diverge from the canonical
/// `platform/abi/families.toml`). At image-build time the platform tooling derives THIS device's
/// family row (canonical id + accepted aliases + the ABI versions this Platform offers) into a
/// small on-device config the supervisor reads via [`load`](Self::load); the broker carries no
/// family names of its own. See `platform/abi/families.toml` / `docs/PLATFORM-ABI-CONTRACT.md`.
///
/// On-device config shape (`[platform]` table):
/// ```toml
/// [platform]
/// family        = "pocketforge/a133-powervr"
/// aliases       = ["pocketforge/sun50i-a133"]
/// supported-abi = ["1"]
/// ```
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PlatformContext {
    /// This Platform's canonical family id (matches `families.toml` `[[family]] id`).
    pub family: String,
    /// The accepted alias ids for this family (matches `families.toml` `[[family]] alias`); an app
    /// that pinned a superseded id still resolves.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// The frozen-ABI versions this Platform offers (e.g. `["1"]`).
    #[serde(default, rename = "supported-abi")]
    pub supported_abi: Vec<String>,
}

/// The on-device platform-advertisement file: a single `[platform]` table wrapping a
/// [`PlatformContext`], so the supervisor can `PlatformContext::load("…/platform.toml")`.
#[derive(Debug, Deserialize)]
struct PlatformFile {
    platform: PlatformContext,
}

impl PlatformContext {
    /// Parse a platform advertisement from a `[platform]`-table TOML string.
    pub fn from_toml(s: &str) -> Result<PlatformContext, toml::de::Error> {
        toml::from_str::<PlatformFile>(s).map(|f| f.platform)
    }

    /// Load the platform advertisement from an on-device `platform.toml` path (the supervisor's
    /// family-advertisement source; derived from `platform/abi/families.toml` at image-build time).
    pub fn load(path: impl AsRef<std::path::Path>) -> std::io::Result<PlatformContext> {
        let text = std::fs::read_to_string(path)?;
        PlatformContext::from_toml(&text)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
    }

    /// Does this Platform accept `app_family` — its canonical id OR an accepted alias?
    pub fn family_matches(&self, app_family: &str) -> bool {
        self.family == app_family || self.aliases.iter().any(|a| a == app_family)
    }

    /// Does this Platform offer `abi`?
    pub fn offers_abi(&self, abi: &str) -> bool {
        self.supported_abi.iter().any(|a| a == abi)
    }
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

    /// Validate the app's `[runtime]` Platform pin against the running Platform's own family
    /// advertisement (`platform`) — the **cooperative on-device launch-time family/abi match** (the
    /// third consumer of the one canonical descriptor, next to the `use=[]` graph check above and
    /// the static package-time validator in the `platform` repo).
    ///
    /// Rejects (collecting ALL applicable violations, not just the first):
    ///   * [`Violation::MissingRuntime`] — no `[runtime]` table (this launch requires a pin);
    ///   * [`Violation::FamilyMismatch`] — the pinned `family` is neither this Platform's canonical
    ///     family nor an accepted alias (a build for a different SoC family);
    ///   * [`Violation::UnsupportedAbi`] — this Platform does not offer the pinned `abi`.
    ///
    /// Accepts (returning the pin) a `family` that matches canonical OR alias AND an offered `abi`.
    /// `platform-version` is deliberately NOT checked here (out-of-lock-version is the static
    /// package-time validator's job; the on-device broker holds no `platform.lock`).
    ///
    /// Parsing an `app.toml` with no `[runtime]` still succeeds ([`from_toml`](Self::from_toml)) —
    /// back-compat lives at the parse layer; a supervisor that wants to allow un-pinned legacy apps
    /// simply skips this check (or gates on [`self.runtime`](Self::runtime)`.is_some()`).
    pub fn check_runtime(
        &self,
        platform: &PlatformContext,
    ) -> Result<&RuntimeSection, Vec<Violation>> {
        let rt = match &self.runtime {
            Some(rt) => rt,
            None => return Err(vec![Violation::MissingRuntime]),
        };
        let mut violations = Vec::new();
        if !platform.family_matches(&rt.family) {
            violations.push(Violation::FamilyMismatch {
                app_family: rt.family.clone(),
                platform_family: platform.family.clone(),
            });
        }
        if !platform.offers_abi(&rt.abi) {
            violations.push(Violation::UnsupportedAbi { abi: rt.abi.clone() });
        }
        if violations.is_empty() {
            Ok(rt)
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
            runtime: None,
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

    // --- [runtime] family/abi cooperative launch-time match (tsp-ziac.5) -------------------------

    /// This Platform is the a133-powervr family, advertised (as the supervisor would derive it from
    /// `platform/abi/families.toml`) with its accepted alias + the ABI versions it offers.
    fn a133_platform() -> PlatformContext {
        PlatformContext::from_toml(
            "[platform]\n\
             family = \"pocketforge/a133-powervr\"\n\
             aliases = [\"pocketforge/sun50i-a133\"]\n\
             supported-abi = [\"1\"]\n",
        )
        .expect("platform advertisement parses")
    }

    /// An `app.toml` carrying an `[app]` + `[runtime]` pin, parsed the same way the supervisor does.
    fn app_with_runtime(family: &str, abi: &str) -> AppManifest {
        AppManifest::from_toml(&format!(
            "[app]\n\
             id = \"com.test.app\"\n\
             use = [\"input\"]\n\
             [runtime]\n\
             family = \"{family}\"\n\
             abi = \"{abi}\"\n\
             platform-version = \"1\"\n",
        ))
        .expect("app.toml with [runtime] parses")
    }

    #[test]
    fn runtime_section_parses_all_fields() {
        let m = app_with_runtime("pocketforge/a133-powervr", "1");
        let rt = m.runtime.as_ref().expect("[runtime] present");
        assert_eq!(rt.family, "pocketforge/a133-powervr");
        assert_eq!(rt.abi, "1");
        assert_eq!(rt.platform_version.as_deref(), Some("1"));
    }

    #[test]
    fn back_compat_no_runtime_table_still_parses() {
        // An app.toml written before the pin existed carries no [runtime]: parse succeeds, runtime None.
        let m = AppManifest::from_toml("[app]\nid = \"com.legacy.app\"\nuse = [\"input\"]\n")
            .expect("legacy app.toml parses");
        assert!(m.runtime.is_none(), "missing [runtime] stays None (back-compat)");
        // …but a launch that requires the pin gets MissingRuntime from check_runtime.
        assert_eq!(
            m.check_runtime(&a133_platform()).unwrap_err(),
            vec![Violation::MissingRuntime]
        );
    }

    #[test]
    fn check_runtime_accepts_canonical_family_match() {
        let m = app_with_runtime("pocketforge/a133-powervr", "1");
        let rt = m.check_runtime(&a133_platform()).expect("canonical family + offered abi accepted");
        assert_eq!(rt.family, "pocketforge/a133-powervr");
    }

    #[test]
    fn check_runtime_accepts_alias_family_match() {
        // An app that pinned the E2 draft SoC-only id resolves against the Platform's accepted alias.
        let m = app_with_runtime("pocketforge/sun50i-a133", "1");
        m.check_runtime(&a133_platform()).expect("alias family accepted");
    }

    #[test]
    fn check_runtime_rejects_family_mismatch() {
        // A build for the OTHER SoC family (different kernel/GPU/SDL) is rejected on this Platform.
        let m = app_with_runtime("pocketforge/a523-mali", "1");
        assert_eq!(
            m.check_runtime(&a133_platform()).unwrap_err(),
            vec![Violation::FamilyMismatch {
                app_family: "pocketforge/a523-mali".into(),
                platform_family: "pocketforge/a133-powervr".into(),
            }]
        );
    }

    #[test]
    fn check_runtime_rejects_unsupported_abi() {
        // Right family, but an ABI version this Platform does not offer.
        let m = app_with_runtime("pocketforge/a133-powervr", "2");
        assert_eq!(
            m.check_runtime(&a133_platform()).unwrap_err(),
            vec![Violation::UnsupportedAbi { abi: "2".into() }]
        );
    }

    #[test]
    fn check_runtime_reports_both_family_and_abi_violations() {
        // Wrong family AND unoffered abi → BOTH violations collected (not just the first).
        let m = app_with_runtime("pocketforge/a523-mali", "9");
        let errs = m.check_runtime(&a133_platform()).unwrap_err();
        assert!(errs.contains(&Violation::FamilyMismatch {
            app_family: "pocketforge/a523-mali".into(),
            platform_family: "pocketforge/a133-powervr".into(),
        }));
        assert!(errs.contains(&Violation::UnsupportedAbi { abi: "9".into() }));
    }

    #[test]
    fn platform_context_loads_from_config_not_a_hardcoded_registry() {
        // The family-advertisement source is device/platform config (parsed here), NOT Rust constants.
        let p = a133_platform();
        assert!(p.family_matches("pocketforge/a133-powervr"));
        assert!(p.family_matches("pocketforge/sun50i-a133"), "alias resolves");
        assert!(!p.family_matches("pocketforge/a523-mali"));
        assert!(p.offers_abi("1"));
        assert!(!p.offers_abi("2"));
    }
}
