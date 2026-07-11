//! [`PrefsStore`] — the persistent home of the preference document.
//!
//! ## Where it lives (follows the AppOps store family, does not fork it)
//!
//! A single JSON document at `$PF_PREFS_DIR/prefs.json` (owner ruling Q2), resolving the
//! directory exactly the way `pf_broker::appops` resolves its own: `$PF_PREFS_DIR` if set, else
//! `$XDG_STATE_HOME/pocketforge/prefs`, else `$HOME/.local/state/pocketforge/prefs`, else a temp
//! fallback. `PF_PREFS_DIR` mirrors `PF_APPOPS_DIR` so tests can point the store at a scratch dir.
//! Unlike the AppOps *ledger* (an append-only event log), preferences are fit-for-current-state:
//! one JSON object holding the live values, rewritten whole. That is the right shape for a small
//! set of user toggles a human should be able to `cat` and hand-edit.
//!
//! ## Durability
//!
//! Writes are atomic: the document is serialized to a sibling `prefs.json.tmp.<pid>`, fsynced,
//! then `rename(2)`d over `prefs.json` (rename is atomic within a directory on local
//! filesystems). A crash mid-write leaves either the old document or the new one, never a torn
//! file — and a reader never observes the temp.
//!
//! ## Tolerant load
//!
//! [`PrefsStore::load`] never panics: a missing file yields all-defaults; a present file is
//! parsed as a JSON object and every known key validated against the schema (a type mismatch or
//! out-of-range scalar is a typed [`PrefError`], surfaced not swallowed); unknown keys are
//! *preserved* into the forward-compat tail rather than rejected, so an older reader round-trips
//! a newer writer's key instead of dropping it.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::error::PrefError;
use crate::prefs::{PrefChange, Prefs};
use crate::schema::{self, PrefKind, PrefValue};

/// The bare document filename inside the resolved directory.
const PREFS_FILE: &str = "prefs.json";

/// A handle to the on-disk preference document.
#[derive(Debug, Clone)]
pub struct PrefsStore {
    path: PathBuf,
}

impl PrefsStore {
    /// Open the store at the platform-default location (honoring `$PF_PREFS_DIR`).
    pub fn open_default() -> PrefsStore {
        PrefsStore::at(default_prefs_dir())
    }

    /// Open the store under an explicit directory (the document is `<dir>/prefs.json`).
    pub fn at(dir: impl AsRef<Path>) -> PrefsStore {
        PrefsStore { path: dir.as_ref().join(PREFS_FILE) }
    }

    /// The full path to the JSON document.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the current document (tolerant — see module docs). A missing file is all-defaults.
    pub fn load(&self) -> Result<Prefs, PrefError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Prefs::defaults()),
            Err(e) => return Err(PrefError::Io(e)),
        };
        let text = String::from_utf8(bytes)
            .map_err(|e| PrefError::Parse(format!("not valid UTF-8: {e}")))?;
        // An empty/whitespace-only file is treated as "no values yet" => defaults (tolerant).
        if text.trim().is_empty() {
            return Ok(Prefs::defaults());
        }
        let value: Value =
            serde_json::from_str(&text).map_err(|e| PrefError::Parse(e.to_string()))?;
        let obj = value
            .as_object()
            .ok_or_else(|| PrefError::Parse("top-level JSON value is not an object".to_string()))?;
        decode(obj)
    }

    /// Atomically persist a document (temp write + fsync + rename). Creates the directory if
    /// needed.
    pub fn save(&self, prefs: &Prefs) -> Result<(), PrefError> {
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir)?;

        let text = encode(prefs);
        let tmp = dir.join(format!("{PREFS_FILE}.tmp.{}", std::process::id()));

        // Write + fsync the temp, then atomically rename it over the live document.
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        match std::fs::rename(&tmp, &self.path) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Best-effort cleanup so a failed rename does not leave a temp behind.
                let _ = std::fs::remove_file(&tmp);
                Err(PrefError::Io(e))
            }
        }
    }

    /// The persist-and-signal write seam: load → set one key → save, returning the
    /// [`PrefChange`] iff the effective value moved. This is the single write path the
    /// `pf-settings` CLI (and, in `.2`, the settings authority + `PrefsDidChange` observer) go
    /// through.
    pub fn apply(&self, key: &str, value: PrefValue) -> Result<Option<PrefChange>, PrefError> {
        let mut prefs = self.load()?;
        let change = prefs.set(key, value)?;
        self.save(&prefs)?;
        Ok(change)
    }
}

/// Resolve the store directory, mirroring `pf_broker::appops::default_appops_dir`.
fn default_prefs_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("PF_PREFS_DIR") {
        return PathBuf::from(v);
    }
    if let Some(base) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(base).join("pocketforge").join("prefs");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state").join("pocketforge").join("prefs");
    }
    std::env::temp_dir().join("pocketforge-prefs")
}

/// Serialize a document to a pretty, deterministically-ordered JSON object with a trailing
/// newline. Only explicitly-stored values are written (a fresh store is `{}`); preserved unknown
/// keys are written back so forward-compat keys survive.
fn encode(prefs: &Prefs) -> String {
    // BTreeMap iteration is sorted, so the JSON key order is stable/diffable.
    let mut map = Map::new();
    for (k, v) in &prefs.stored {
        map.insert(k.clone(), pref_to_json(*v));
    }
    for (k, v) in &prefs.extra {
        // Never let a preserved unknown key shadow a real one (a real key always wins).
        map.entry(k.clone()).or_insert_with(|| v.clone());
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(map))
        .expect("a Map<String, Value> always serializes");
    text.push('\n');
    text
}

/// Decode a parsed JSON object into a validated document.
fn decode(obj: &Map<String, Value>) -> Result<Prefs, PrefError> {
    let mut prefs = Prefs::defaults();
    for (key, raw) in obj {
        match schema::spec(key) {
            Some(spec) => {
                let value = json_to_pref(key, spec.kind, raw)?;
                // Re-validate through the schema (range check for scalars).
                let validated = spec.validate(value)?;
                prefs.stored.insert(key.clone(), validated);
            }
            // Forward-compat: keep unknown keys instead of erroring or dropping them.
            None => {
                prefs.extra.insert(key.clone(), raw.clone());
            }
        }
    }
    Ok(prefs)
}

/// A concrete value → its JSON encoding.
fn pref_to_json(v: PrefValue) -> Value {
    match v {
        PrefValue::Bool(b) => Value::Bool(b),
        PrefValue::Scalar(n) => Value::Number(n.into()),
    }
}

/// A JSON value → a concrete value for a known key, enforcing the key's type.
fn json_to_pref(key: &str, kind: PrefKind, raw: &Value) -> Result<PrefValue, PrefError> {
    match kind {
        PrefKind::Bool => match raw {
            Value::Bool(b) => Ok(PrefValue::Bool(*b)),
            other => Err(PrefError::Type {
                key: key.to_string(),
                expected: "bool",
                got: json_kind(other),
            }),
        },
        PrefKind::Scalar { .. } => match raw.as_i64() {
            // as_i64 rejects floats/strings/overflow — exactly the "malformed scalar" cases.
            Some(n) if raw.is_i64() || raw.is_u64() => Ok(PrefValue::Scalar(n)),
            _ => Err(PrefError::Type {
                key: key.to_string(),
                expected: "scalar",
                got: json_kind(raw),
            }),
        },
    }
}

/// Human name for a JSON value's shape, for type-mismatch diagnostics.
fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch store dir unique to this process+tag (no external temp-crate dep).
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("pf-prefs-test-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn missing_file_loads_all_defaults() {
        let store = PrefsStore::at(scratch("missing"));
        let p = store.load().unwrap();
        assert_eq!(p, Prefs::defaults());
        assert!(p.haptics_enabled());
    }

    #[test]
    fn atomic_write_round_trips() {
        let dir = scratch("roundtrip");
        let store = PrefsStore::at(&dir);
        store.apply("hapticsEnabled", PrefValue::Bool(false)).unwrap();
        store.apply("brightness", PrefValue::Scalar(42)).unwrap();

        let reloaded = store.load().unwrap();
        assert!(!reloaded.haptics_enabled());
        assert_eq!(reloaded.brightness(), 42);
        assert_eq!(reloaded.source("hapticsEnabled"), crate::prefs::Source::Stored);
        assert_eq!(reloaded.source("monoAudio"), crate::prefs::Source::Default);

        // Only explicitly-set keys are written; a fresh key stays default.
        assert!(store.path().exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_signals_only_on_effective_change() {
        let dir = scratch("signal");
        let store = PrefsStore::at(&dir);
        assert!(store.apply("hapticsEnabled", PrefValue::Bool(false)).unwrap().is_some());
        // Re-setting the same value: no effective change => no signal.
        assert!(store.apply("hapticsEnabled", PrefValue::Bool(false)).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_json_is_a_typed_error_not_a_panic() {
        let dir = scratch("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("prefs.json"), b"{ not json").unwrap();
        let store = PrefsStore::at(&dir);
        assert!(matches!(store.load(), Err(PrefError::Parse(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_object_top_level_is_a_typed_error() {
        let dir = scratch("array");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("prefs.json"), b"[1,2,3]").unwrap();
        let store = PrefsStore::at(&dir);
        assert!(matches!(store.load(), Err(PrefError::Parse(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn type_mismatch_in_stored_doc_is_a_typed_error() {
        let dir = scratch("mismatch");
        std::fs::create_dir_all(&dir).unwrap();
        // brightness as a string is malformed.
        std::fs::write(dir.join("prefs.json"), br#"{"brightness":"bright"}"#).unwrap();
        let store = PrefsStore::at(&dir);
        assert!(matches!(store.load(), Err(PrefError::Type { .. })));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_range_scalar_in_stored_doc_is_a_typed_error() {
        let dir = scratch("oorange");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("prefs.json"), br#"{"brightness":250}"#).unwrap();
        let store = PrefsStore::at(&dir);
        assert!(matches!(store.load(), Err(PrefError::Range { .. })));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_keys_are_preserved_across_round_trip() {
        let dir = scratch("forward");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("prefs.json"),
            br#"{"hapticsEnabled":false,"futureKnob":"v2-only"}"#,
        )
        .unwrap();
        let store = PrefsStore::at(&dir);
        let p = store.load().unwrap();
        assert!(!p.haptics_enabled());
        assert_eq!(p.extra().get("futureKnob").and_then(|v| v.as_str()), Some("v2-only"));
        // Re-save and confirm the unknown key survived.
        store.save(&p).unwrap();
        let reloaded = store.load().unwrap();
        assert_eq!(reloaded.extra().get("futureKnob").and_then(|v| v.as_str()), Some("v2-only"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_file_is_tolerated_as_defaults() {
        let dir = scratch("empty");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("prefs.json"), b"   \n").unwrap();
        let store = PrefsStore::at(&dir);
        assert_eq!(store.load().unwrap(), Prefs::defaults());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
