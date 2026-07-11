//! [`Prefs`] — the in-memory current-state preference document: the effective value of every
//! preference (an explicitly-stored value, else the schema default) plus a read-API and a pure
//! `set` seam that reports *what changed*.
//!
//! The `set` seam is deliberately split from persistence. `set` mutates the in-memory document
//! and returns a [`PrefChange`] iff the effective value moved; [`crate::store::PrefsStore`] wires
//! that into load→set→save. `.2` attaches its `PrefsDidChange` observer to the same
//! change-report — a fired `Some(PrefChange)` is precisely the "notify running apps" signal, so
//! the observer hook is a one-line graft onto the write path, not a redesign.

use std::collections::BTreeMap;

use crate::error::PrefError;
use crate::schema::{self, PrefValue};

/// Where an effective preference value came from — used by `pf-settings list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// No stored value; the schema default is in effect.
    Default,
    /// A value explicitly written to the store is in effect.
    Stored,
}

impl Source {
    /// Lowercase label for display.
    pub fn label(&self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::Stored => "stored",
        }
    }
}

/// What a write changed: the key and its effective value before/after. This is the
/// persist-and-signal payload `.2`'s `PrefsDidChange` observer consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefChange {
    /// The preference key that changed.
    pub key: String,
    /// The effective value before the write (stored-or-default).
    pub old: PrefValue,
    /// The effective value after the write.
    pub new: PrefValue,
}

/// The current-state preference document.
///
/// `stored` holds only the keys a user/authority explicitly set; every other known key reads
/// through to its schema default. `extra` preserves unknown keys encountered on load
/// (forward-compat: a newer writer's key survives an older reader's round-trip instead of being
/// silently dropped).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Prefs {
    pub(crate) stored: BTreeMap<String, PrefValue>,
    pub(crate) extra: BTreeMap<String, serde_json::Value>,
}

impl Prefs {
    /// An all-defaults document (the state of a missing store file).
    pub fn defaults() -> Prefs {
        Prefs::default()
    }

    /// The effective value of a known key: its stored value, else the schema default.
    ///
    /// Returns [`PrefError::UnknownKey`] for a key not in the schema — callers that want a raw
    /// preserved value should read [`Prefs::extra`] directly.
    pub fn value(&self, key: &str) -> Result<PrefValue, PrefError> {
        let spec = schema::spec(key).ok_or_else(|| PrefError::UnknownKey(key.to_string()))?;
        Ok(self.stored.get(key).copied().unwrap_or(spec.default))
    }

    /// Where a known key's effective value comes from (default vs stored).
    pub fn source(&self, key: &str) -> Source {
        if self.stored.contains_key(key) {
            Source::Stored
        } else {
            Source::Default
        }
    }

    /// Typed bool read. Errors if the key is unknown or is not a bool preference.
    pub fn get_bool(&self, key: &str) -> Result<bool, PrefError> {
        match self.value(key)? {
            PrefValue::Bool(b) => Ok(b),
            other => Err(PrefError::Type {
                key: key.to_string(),
                expected: "bool",
                got: other.kind_name(),
            }),
        }
    }

    /// Typed scalar read. Errors if the key is unknown or is not a scalar preference.
    pub fn get_scalar(&self, key: &str) -> Result<i64, PrefError> {
        match self.value(key)? {
            PrefValue::Scalar(n) => Ok(n),
            other => Err(PrefError::Type {
                key: key.to_string(),
                expected: "scalar",
                got: other.kind_name(),
            }),
        }
    }

    // --- Named typed getters for the v1 schema (the facade's read-API surface) -------------
    // These are infallible: the keys and types are schema constants, so they cannot error.

    /// `reduceMotion` — suppress non-essential cosmetic motion.
    pub fn reduce_motion(&self) -> bool {
        self.get_bool("reduceMotion").unwrap_or(false)
    }

    /// `hapticsEnabled` — allow haptic/rumble actuation (honored at the primitive).
    pub fn haptics_enabled(&self) -> bool {
        self.get_bool("hapticsEnabled").unwrap_or(true)
    }

    /// `monoAudio` — down-mix audio to mono.
    pub fn mono_audio(&self) -> bool {
        self.get_bool("monoAudio").unwrap_or(false)
    }

    /// `brightness` — 0..=100 (CONTRACT-ONLY in v1; no sysfs apply leg in this epic).
    pub fn brightness(&self) -> i64 {
        self.get_scalar("brightness").unwrap_or(100)
    }

    /// Validate-and-set a preference in memory. Rejects unknown keys, type mismatches, and
    /// out-of-range scalars ([`PrefError`]). The explicit set is always recorded (so its
    /// [`Source`] becomes [`Source::Stored`]); the returned [`PrefChange`] is `Some` **iff the
    /// effective value actually moved** — that is the "fire the observer" signal `.2` keys off.
    pub fn set(&mut self, key: &str, value: PrefValue) -> Result<Option<PrefChange>, PrefError> {
        let validated = schema::validate(key, value)?;
        let old = self.value(key)?; // safe: validate() proved the key is known
        self.stored.insert(key.to_string(), validated);
        Ok(if old != validated {
            Some(PrefChange { key: key.to_string(), old, new: validated })
        } else {
            None
        })
    }

    /// Preserved unknown keys (forward-compat tail). Not exposed as typed values.
    pub fn extra(&self) -> &BTreeMap<String, serde_json::Value> {
        &self.extra
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_read_through_the_schema() {
        let p = Prefs::defaults();
        assert!(!p.reduce_motion());
        assert!(p.haptics_enabled());
        assert!(!p.mono_audio());
        assert_eq!(p.brightness(), 100);
        assert_eq!(p.source("hapticsEnabled"), Source::Default);
    }

    #[test]
    fn set_reports_a_change_and_flips_source() {
        let mut p = Prefs::defaults();
        let change = p.set("hapticsEnabled", PrefValue::Bool(false)).unwrap();
        assert_eq!(
            change,
            Some(PrefChange {
                key: "hapticsEnabled".into(),
                old: PrefValue::Bool(true),
                new: PrefValue::Bool(false),
            })
        );
        assert!(!p.haptics_enabled());
        assert_eq!(p.source("hapticsEnabled"), Source::Stored);
    }

    #[test]
    fn set_to_the_same_effective_value_reports_no_change() {
        let mut p = Prefs::defaults();
        // hapticsEnabled default is true; setting it true is a no-op change (but still stored).
        assert_eq!(p.set("hapticsEnabled", PrefValue::Bool(true)).unwrap(), None);
        assert_eq!(p.source("hapticsEnabled"), Source::Stored);
    }

    #[test]
    fn set_validates() {
        let mut p = Prefs::defaults();
        assert!(matches!(p.set("brightness", PrefValue::Scalar(200)), Err(PrefError::Range { .. })));
        assert!(matches!(p.set("brightness", PrefValue::Bool(true)), Err(PrefError::Type { .. })));
        assert!(matches!(p.set("bogus", PrefValue::Bool(true)), Err(PrefError::UnknownKey(_))));
    }
}
