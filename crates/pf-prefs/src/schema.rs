//! The preference **schema as data** — the single source of truth for which preferences exist,
//! their type, their default, and (for scalars) their valid range.
//!
//! Keeping the schema a `const` table (not scattered `if key == "…"` arms) is what makes the
//! *extensible tail* cheap: adding a preference is one [`PrefSpec`] row; the validator, the
//! read-API defaults, and the `pf-settings list` view all derive from this table with no other
//! edit. Every value that flows through the store is validated against a spec here — a stored
//! document can never carry an out-of-range scalar or a type-mismatched key (they become a typed
//! [`crate::PrefError`], never a panic).

use crate::error::PrefError;

/// The runtime type of a preference value.
///
/// The v1 schema needs only `Bool` and a bounded `Scalar`; the enum is the extension point —
/// a future `Enum { variants: &[&str] }` (e.g. a `theme` preference) slots in here and every
/// consumer that already matches on `PrefKind` keeps compiling with one new arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefKind {
    /// An on/off preference.
    Bool,
    /// An integer preference constrained to the inclusive range `[min, max]`.
    Scalar { min: i64, max: i64 },
}

/// A concrete, validated preference value. Copy-cheap; the store round-trips these to JSON
/// (`Bool` ↔ JSON bool, `Scalar` ↔ JSON number).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefValue {
    /// An on/off value.
    Bool(bool),
    /// An integer value (already validated against its key's [`PrefKind::Scalar`] range when it
    /// came through [`validate`]).
    Scalar(i64),
}

impl PrefValue {
    /// The kind this value inhabits, used for type-match diagnostics.
    pub fn kind_name(&self) -> &'static str {
        match self {
            PrefValue::Bool(_) => "bool",
            PrefValue::Scalar(_) => "scalar",
        }
    }
}

impl std::fmt::Display for PrefValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrefValue::Bool(b) => write!(f, "{b}"),
            PrefValue::Scalar(n) => write!(f, "{n}"),
        }
    }
}

/// One row of the schema: a preference's key, type, default, and human doc.
#[derive(Debug, Clone, Copy)]
pub struct PrefSpec {
    /// The canonical camelCase key (matches the merged in-memory seam, e.g. `hapticsEnabled`).
    pub key: &'static str,
    /// The value type + (for scalars) valid range.
    pub kind: PrefKind,
    /// The value returned when the key is absent from the stored document.
    pub default: PrefValue,
    /// One-line human description (surfaced by `pf-settings list`).
    pub doc: &'static str,
}

impl PrefSpec {
    /// Validate a candidate value against this spec's type and range.
    pub fn validate(&self, value: PrefValue) -> Result<PrefValue, PrefError> {
        match (self.kind, value) {
            (PrefKind::Bool, PrefValue::Bool(_)) => Ok(value),
            (PrefKind::Scalar { min, max }, PrefValue::Scalar(n)) => {
                if n < min || n > max {
                    Err(PrefError::Range { key: self.key.to_string(), value: n, min, max })
                } else {
                    Ok(value)
                }
            }
            // Type mismatch: the stored/attempted value's shape does not match the schema.
            (kind, got) => Err(PrefError::Type {
                key: self.key.to_string(),
                expected: kind_name(kind),
                got: got.kind_name(),
            }),
        }
    }
}

/// Human name for a [`PrefKind`], used in type-mismatch errors.
pub fn kind_name(kind: PrefKind) -> &'static str {
    match kind {
        PrefKind::Bool => "bool",
        PrefKind::Scalar { .. } => "scalar",
    }
}

/// The v1 preference schema.
///
/// Defaults are chosen to match the already-merged in-memory seam (`hapticsEnabled` defaults ON,
/// as `InProcessBackend::rumble_pulse` reads it) and the accessibility-off-by-default norm
/// (`reduceMotion`/`monoAudio` default off — the accessible affordance is opt-in, never
/// surprising). `brightness` is CONTRACT-ONLY in v1 per owner ruling Q3: a scalar the facade
/// reads and the observer fires on, with **no sysfs apply leg anywhere in this epic** (a133 has
/// no `/sys/class/backlight`; the apply leg is a hardware-gated follow-on).
pub const SCHEMA: &[PrefSpec] = &[
    PrefSpec {
        key: "reduceMotion",
        kind: PrefKind::Bool,
        default: PrefValue::Bool(false),
        doc: "Suppress non-essential cosmetic motion/animation.",
    },
    PrefSpec {
        key: "hapticsEnabled",
        kind: PrefKind::Bool,
        default: PrefValue::Bool(true),
        doc: "Allow haptic/rumble actuation (honored at the primitive; off => silent no-op).",
    },
    PrefSpec {
        key: "monoAudio",
        kind: PrefKind::Bool,
        default: PrefValue::Bool(false),
        doc: "Down-mix audio to mono for single-ear/hearing accessibility.",
    },
    PrefSpec {
        key: "brightness",
        kind: PrefKind::Scalar { min: 0, max: 100 },
        default: PrefValue::Scalar(100),
        doc: "Display brightness 0..=100 (CONTRACT-ONLY in v1; no sysfs apply leg).",
    },
];

/// Look up the spec for a key, if it is a known preference.
pub fn spec(key: &str) -> Option<&'static PrefSpec> {
    SCHEMA.iter().find(|s| s.key == key)
}

/// Validate a value against the schema for `key`.
///
/// Unknown keys are rejected here ([`PrefError::UnknownKey`]) — this is the *explicit set* path
/// (CLI / write authority), which must never invent a key. The tolerant-load path in
/// [`crate::store`] handles unknown keys differently (it *preserves* them for forward-compat
/// rather than erroring); see that module's docs.
pub fn validate(key: &str, value: PrefValue) -> Result<PrefValue, PrefError> {
    match spec(key) {
        Some(s) => s.validate(value),
        None => Err(PrefError::UnknownKey(key.to_string())),
    }
}

/// Parse a raw command-line string into a validated [`PrefValue`] for `key`, driven by the
/// key's schema type. Bool keys accept `true`/`false`/`on`/`off`/`1`/`0` (case-insensitive);
/// scalar keys accept a base-10 integer and are range-checked. Unknown keys, unparseable
/// tokens, and out-of-range values all yield a typed [`PrefError`].
pub fn parse_value(key: &str, raw: &str) -> Result<PrefValue, PrefError> {
    let spec = spec(key).ok_or_else(|| PrefError::UnknownKey(key.to_string()))?;
    let value = match spec.kind {
        PrefKind::Bool => match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "on" | "1" | "yes" => PrefValue::Bool(true),
            "false" | "off" | "0" | "no" => PrefValue::Bool(false),
            _ => {
                return Err(PrefError::Type {
                    key: key.to_string(),
                    expected: "bool",
                    got: "unparseable",
                })
            }
        },
        PrefKind::Scalar { .. } => match raw.trim().parse::<i64>() {
            Ok(n) => PrefValue::Scalar(n),
            Err(_) => {
                return Err(PrefError::Type {
                    key: key.to_string(),
                    expected: "scalar",
                    got: "unparseable",
                })
            }
        },
    };
    spec.validate(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_value_bool_forms() {
        assert_eq!(parse_value("hapticsEnabled", "false").unwrap(), PrefValue::Bool(false));
        assert_eq!(parse_value("hapticsEnabled", "ON").unwrap(), PrefValue::Bool(true));
        assert_eq!(parse_value("hapticsEnabled", "0").unwrap(), PrefValue::Bool(false));
        assert!(parse_value("hapticsEnabled", "maybe").is_err());
    }

    #[test]
    fn parse_value_scalar_range() {
        assert_eq!(parse_value("brightness", "42").unwrap(), PrefValue::Scalar(42));
        assert!(matches!(parse_value("brightness", "250"), Err(PrefError::Range { .. })));
        assert!(matches!(parse_value("brightness", "notanum"), Err(PrefError::Type { .. })));
    }

    #[test]
    fn parse_value_unknown_key() {
        assert!(matches!(parse_value("bogus", "true"), Err(PrefError::UnknownKey(_))));
    }

    #[test]
    fn schema_defaults_match_the_contract() {
        assert_eq!(spec("reduceMotion").unwrap().default, PrefValue::Bool(false));
        assert_eq!(spec("hapticsEnabled").unwrap().default, PrefValue::Bool(true));
        assert_eq!(spec("monoAudio").unwrap().default, PrefValue::Bool(false));
        assert_eq!(spec("brightness").unwrap().default, PrefValue::Scalar(100));
    }

    #[test]
    fn every_default_is_self_consistent() {
        // A default value must itself validate against its own spec.
        for s in SCHEMA {
            assert!(s.validate(s.default).is_ok(), "default for {} is invalid", s.key);
        }
    }

    #[test]
    fn validate_rejects_unknown_key() {
        assert!(matches!(
            validate("nopeNotAKey", PrefValue::Bool(true)),
            Err(PrefError::UnknownKey(_))
        ));
    }

    #[test]
    fn validate_rejects_type_mismatch() {
        assert!(matches!(
            validate("hapticsEnabled", PrefValue::Scalar(1)),
            Err(PrefError::Type { .. })
        ));
        assert!(matches!(
            validate("brightness", PrefValue::Bool(true)),
            Err(PrefError::Type { .. })
        ));
    }

    #[test]
    fn validate_rejects_out_of_range_scalar() {
        assert!(matches!(
            validate("brightness", PrefValue::Scalar(101)),
            Err(PrefError::Range { .. })
        ));
        assert!(matches!(
            validate("brightness", PrefValue::Scalar(-1)),
            Err(PrefError::Range { .. })
        ));
        assert!(validate("brightness", PrefValue::Scalar(0)).is_ok());
        assert!(validate("brightness", PrefValue::Scalar(100)).is_ok());
    }
}
