//! **Protection tiers** — the normative classification of the capability vocabulary into the
//! policy tiers the launch validator ([`crate::manifest`]) and the enforcing backend
//! ([`crate::enforce`]) act on. This formalizes the *implicit* flat-list split that shipped in the
//! merged v0 broker (`DEFAULT_DENY` / `QUOTA_CAPS` / the `entropy` ungated exception) into an
//! **explicit, documented tier vocabulary** — LOCKED decision #3 (`infra-102`), as refined by R-A
//! and R-C.
//!
//! The three capability tiers (the epic's "three protection tiers ONLY"):
//!
//! * [`Tier::Normal`] — **auto-grant** once declared in `use=[]`: no consent, no quota. The
//!   cosmetic + local caps (`input`, `vibration`/`rumble`, `leds`, `audio`, `settings`) and the
//!   motion sensors (`imu`/`accelerometer`/`gyroscope`/`magnetometer`). `entropy` is Normal with a
//!   further *ungated* exception (auto-granted even when **undeclared** — see the enforcing backend
//!   and `docs/PERMISSION-MODEL.md`).
//! * [`Tier::Dangerous`] — **default-deny + runtime consent + quota**: the privacy/physical-resource
//!   caps `location` and `gnss`, and a *host-scoped* `egress:<host>` (a declared, accounted network
//!   send). This is exactly the merged `DEFAULT_DENY`/`QUOTA_CAPS` set plus scoped egress.
//! * [`Tier::Signature`] — **first-party-only**: raw/arbitrary egress (`egress:0.0.0.0/0`, `::/0`,
//!   `*`, or unscoped `egress`) and any future first-party-only cap. A Signature-tier entry from an
//!   app that is neither first-party-signed nor covered by a **blessed-binary** exemption (R-C) is a
//!   launch **REJECT** ([`crate::manifest::Violation::SignatureTierRequiresTrust`]).
//!
//! **Blessed-binary is NOT a fourth capability tier** — it is a property of the *app* (an
//! enumerated, signed broad-grant), modeled as [`crate::manifest::LaunchTrust`] /
//! [`crate::manifest::BlessedRegistration`], the validator's canonical exemption case. R-C calls it
//! a "4th tier"; we reconcile that with the epic's "three tiers only" by classifying *capabilities*
//! into three tiers and classifying the *app's trust* separately. See `docs/PERMISSION-MODEL.md`.
//!
//! Tier membership is **grounded in the merged `enforce.rs` behavior** so the model formalizes what
//! already ships rather than drifting it: `Dangerous` == the merged `DEFAULT_DENY`/`QUOTA_CAPS`
//! (`location`, `gnss`); `entropy` == the merged `UNGATED`; everything else auto-grants == `Normal`.

use crate::manifest::EGRESS_CAP;

/// The protection tier of a capability (optionally with its scope modifier). See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Auto-grant once declared: no consent, no quota (cosmetic/local caps, motion sensors,
    /// entropy). `entropy` additionally carries the *ungated* exception (see the enforcing backend).
    Normal,
    /// Default-deny + runtime consent + per-capability quota (`location`, `gnss`, scoped egress).
    Dangerous,
    /// First-party-only (raw/arbitrary egress + future first-party-only caps). A Signature-tier
    /// entry from a non-first-party, non-blessed app is a launch reject.
    Signature,
}

impl Tier {
    /// Does this tier require the app to carry first-party (or blessed-binary) signing authority to
    /// *declare* the capability at launch? True only for [`Tier::Signature`].
    pub fn requires_signing_authority(self) -> bool {
        matches!(self, Tier::Signature)
    }
}

/// The classification of a capability `(cap, modifier)` into its protection [`Tier`].
///
/// `modifier` matters only for `egress`, whose tier depends on the destination scope: a specific
/// declared host is [`Tier::Dangerous`] (accounted), while a whole-internet wildcard or an unscoped
/// `egress` is [`Tier::Signature`] (raw/arbitrary — first-party-only). Every other known capability
/// classifies by name alone. (Unknown capabilities never reach here — the validator rejects them
/// with [`crate::manifest::Violation::UnknownCapability`] first.)
pub fn tier_of(cap: &str, modifier: Option<&str>) -> Tier {
    match cap.to_ascii_lowercase().as_str() {
        // Privacy / physical-resource caps: default-deny + consent + quota (merged DEFAULT_DENY).
        "location" | "gnss" => Tier::Dangerous,
        // Egress tier depends on the destination scope.
        EGRESS_CAP => match modifier {
            // Unscoped egress = raw/arbitrary network send ⇒ first-party-only.
            None => Tier::Signature,
            Some(host) if is_broad_egress(host) => Tier::Signature,
            // A specific declared host ⇒ dangerous (declared + accounted; `.4` owns byte ledger).
            Some(_) => Tier::Dangerous,
        },
        // Everything else the platform knows about auto-grants once declared: input, the vibration/
        // rumble + leds cosmetic actuators, audio (playback), settings, entropy, and the motion
        // sensors. (Motion sensors are a KNOWN future dangerous-tier candidate — side-channel /
        // inference — but ship Normal to match the merged auto-grant behavior; see PERMISSION-MODEL.)
        _ => Tier::Normal,
    }
}

/// Is `host` a whole-internet / wildcard egress destination (raw/arbitrary egress, first-party-only)
/// rather than a specific declared host? Recognizes the IPv4/IPv6 default routes and the `*`
/// wildcard.
pub fn is_broad_egress(host: &str) -> bool {
    matches!(host.trim(), "0.0.0.0/0" | "0.0.0.0" | "::/0" | "::" | "*")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dangerous_tier_matches_merged_default_deny() {
        // Exactly the merged DEFAULT_DENY / QUOTA_CAPS set.
        assert_eq!(tier_of("location", Some("approximate")), Tier::Dangerous);
        assert_eq!(tier_of("gnss", None), Tier::Dangerous);
    }

    #[test]
    fn normal_tier_covers_the_auto_grant_vocabulary() {
        for c in ["input", "vibration", "rumble", "leds", "audio", "settings", "entropy",
                  "imu", "accelerometer", "gyroscope", "magnetometer"] {
            assert_eq!(tier_of(c, None), Tier::Normal, "{c} should be Normal (auto-grant)");
        }
    }

    #[test]
    fn egress_tier_depends_on_scope() {
        // Specific declared host = dangerous (accounted).
        assert_eq!(tier_of("egress", Some("steampowered.com")), Tier::Dangerous);
        assert_eq!(tier_of("egress", Some("tile.example")), Tier::Dangerous);
        // Raw/arbitrary = signature (first-party-only).
        assert_eq!(tier_of("egress", Some("0.0.0.0/0")), Tier::Signature);
        assert_eq!(tier_of("egress", Some("::/0")), Tier::Signature);
        assert_eq!(tier_of("egress", Some("*")), Tier::Signature);
        assert_eq!(tier_of("egress", None), Tier::Signature);
    }

    #[test]
    fn only_signature_requires_signing_authority() {
        assert!(!Tier::Normal.requires_signing_authority());
        assert!(!Tier::Dangerous.requires_signing_authority());
        assert!(Tier::Signature.requires_signing_authority());
    }

    #[test]
    fn broad_egress_recognizes_default_routes() {
        assert!(is_broad_egress("0.0.0.0/0"));
        assert!(is_broad_egress(" ::/0 "));
        assert!(is_broad_egress("*"));
        assert!(!is_broad_egress("steampowered.com"));
        assert!(!is_broad_egress("10.0.0.0/8")); // a specific CIDR is a declared scope, not raw
    }
}
