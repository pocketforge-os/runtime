//! The one typed error the preference layer returns. Every failure mode — a corrupt store, a
//! type-mismatched or out-of-range value, an unknown key on the write path, an IO fault — is a
//! variant here. Nothing in this crate panics on bad input: a malformed on-disk document is a
//! [`PrefError::Parse`], never an unwrap.

/// Why a preference read/validate/write failed.
#[derive(Debug)]
pub enum PrefError {
    /// The store file could not be read or written (permission, disk, rename).
    Io(std::io::Error),
    /// The store file exists but is not a valid JSON object.
    Parse(String),
    /// A value was set for a key that is not in the schema (the *explicit set* path rejects
    /// unknown keys; the tolerant-load path preserves them instead — see [`crate::store`]).
    UnknownKey(String),
    /// A stored/attempted value had the wrong type for its key (e.g. a bool for `brightness`).
    Type { key: String, expected: &'static str, got: &'static str },
    /// A scalar value fell outside its schema range.
    Range { key: String, value: i64, min: i64, max: i64 },
}

impl std::fmt::Display for PrefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrefError::Io(e) => write!(f, "preference store I/O error: {e}"),
            PrefError::Parse(msg) => write!(f, "preference store is not a valid JSON object: {msg}"),
            PrefError::UnknownKey(k) => write!(f, "unknown preference key '{k}'"),
            PrefError::Type { key, expected, got } => {
                write!(f, "preference '{key}' expects a {expected} value, got a {got}")
            }
            PrefError::Range { key, value, min, max } => {
                write!(f, "preference '{key}' value {value} is out of range {min}..={max}")
            }
        }
    }
}

impl std::error::Error for PrefError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PrefError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for PrefError {
    fn from(e: std::io::Error) -> Self {
        PrefError::Io(e)
    }
}
