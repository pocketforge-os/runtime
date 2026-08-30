//! [`PreferencePort`] adapter for the persistent [`pf_prefs`] store.
//!
//! The adapter reloads the store when polled, so an authority-side writer in another process is
//! observable without inventing a second preference store. It also keeps the port truthful:
//! only preferences with an existing runtime apply/observe leg are reported as applied.

use std::collections::{BTreeMap, VecDeque};

use pf_ports::{
    ChangeAuthority, Deadline, EffectivePreference, PreferenceChange, PreferenceChangeResult,
    PreferenceError, PreferenceKey, PreferencePoll, PreferencePort, PreferenceValue,
};
use pf_prefs::{PrefError, PrefValue, PrefsStore, SCHEMA};
use pf_prefsd::{Client, ClientError, ErrorKind};

/// The authority name used by the standard settings control plane.
pub const USER_AUTHORITY: &str = "user";

/// Store-backed implementation of the shell's preference boundary.
pub struct PrefsPreferencePort {
    backend: Backend,
    allowed_authority: ChangeAuthority,
    snapshot: BTreeMap<String, PrefValue>,
    pending: VecDeque<EffectivePreference>,
}

enum Backend {
    Store(PrefsStore),
    Daemon(Client),
}

impl PrefsPreferencePort {
    /// Open an adapter and establish its initial observation snapshot.
    pub fn new(
        store: PrefsStore,
        allowed_authority: ChangeAuthority,
    ) -> Result<Self, PreferenceError> {
        let backend = match std::env::var_os("PF_PREFSD_SOCK") {
            Some(socket) => Backend::Daemon(Client::new(socket)),
            None => Backend::Store(store),
        };
        let snapshot = load_all(&backend)?;
        Ok(Self {
            backend,
            allowed_authority,
            snapshot,
            pending: VecDeque::new(),
        })
    }

    /// Open an adapter for the standard user/settings authority.
    pub fn for_user(store: PrefsStore) -> Result<Self, PreferenceError> {
        Self::new(store, ChangeAuthority(USER_AUTHORITY.into()))
    }

    fn refresh(&mut self) -> Result<(), PreferenceError> {
        let fresh = load_all(&self.backend)?;
        for spec in SCHEMA {
            let old = self
                .snapshot
                .get(spec.key)
                .copied()
                .ok_or(PreferenceError::BackendUnavailable)?;
            let new = fresh
                .get(spec.key)
                .copied()
                .ok_or(PreferenceError::BackendUnavailable)?;
            if old != new {
                self.pending.push_back(effective(spec.key, new));
            }
        }
        self.snapshot = fresh;
        Ok(())
    }
}

impl PreferencePort for PrefsPreferencePort {
    fn read(&self, key: &PreferenceKey) -> Result<Option<EffectivePreference>, PreferenceError> {
        let Some(spec) = pf_prefs::spec(&key.0) else {
            return Ok(None);
        };
        let stored = match &self.backend {
            Backend::Store(_) => self.snapshot.get(spec.key).copied(),
            Backend::Daemon(client) => Some(json_to_pref(
                spec.key,
                client.get(spec.key).map_err(map_client_error)?,
            )?),
        }
        .ok_or(PreferenceError::BackendUnavailable)?;
        Ok(Some(effective(spec.key, stored)))
    }

    fn next_change(&mut self, _deadline: Deadline) -> Result<PreferencePoll, PreferenceError> {
        if let Some(change) = self.pending.pop_front() {
            return Ok(PreferencePoll::Changed(change));
        }
        self.refresh()?;
        Ok(match self.pending.pop_front() {
            Some(change) => PreferencePoll::Changed(change),
            None => PreferencePoll::DeadlineReached,
        })
    }

    fn submit_change(
        &mut self,
        change: PreferenceChange,
    ) -> Result<PreferenceChangeResult, PreferenceError> {
        if change.authority != self.allowed_authority {
            return Ok(PreferenceChangeResult::Unauthorized);
        }
        if pf_prefs::spec(&change.key.0).is_none() {
            return Ok(PreferenceChangeResult::UnsupportedKey);
        }
        let value = from_port_value(&change.key.0, change.value)?;
        match &self.backend {
            Backend::Store(store) => {
                store
                    .apply(&change.key.0, value)
                    .map_err(map_backend_error)?;
            }
            Backend::Daemon(client) => {
                client
                    .set(&change.key.0, pref_to_json(value))
                    .map_err(map_client_set_error)?;
            }
        }
        self.refresh()?;
        Ok(PreferenceChangeResult::StoredNotApplied)
    }
}

fn load_all(backend: &Backend) -> Result<BTreeMap<String, PrefValue>, PreferenceError> {
    match backend {
        Backend::Store(store) => {
            let prefs = store.load().map_err(map_backend_error)?;
            SCHEMA
                .iter()
                .map(|spec| prefs.value(spec.key).map(|value| (spec.key.into(), value)))
                .collect::<Result<_, _>>()
                .map_err(map_backend_error)
        }
        Backend::Daemon(client) => client
            .get_all()
            .map_err(map_client_error)?
            .into_iter()
            .map(|(key, value)| json_to_pref(&key, value).map(|value| (key, value)))
            .collect(),
    }
}

fn pref_to_json(value: PrefValue) -> serde_json::Value {
    match value {
        PrefValue::Bool(value) => value.into(),
        PrefValue::Scalar(value) => value.into(),
        PrefValue::Enum(value) => value.into(),
    }
}

fn json_to_pref(key: &str, value: serde_json::Value) -> Result<PrefValue, PreferenceError> {
    let port_value = match value {
        serde_json::Value::Bool(value) => PreferenceValue::Bool(value),
        serde_json::Value::Number(value) => {
            PreferenceValue::Integer(value.as_i64().ok_or(PreferenceError::BackendUnavailable)?)
        }
        serde_json::Value::String(value) => PreferenceValue::Text(value),
        _ => return Err(PreferenceError::BackendUnavailable),
    };
    from_port_value(key, port_value).map_err(|_| PreferenceError::BackendUnavailable)
}

fn map_client_error(_error: ClientError) -> PreferenceError {
    PreferenceError::BackendUnavailable
}

fn map_client_set_error(error: ClientError) -> PreferenceError {
    match error {
        ClientError::Remote {
            kind: Some(ErrorKind::InvalidValue),
            ..
        } => PreferenceError::InvalidValue,
        error => map_client_error(error),
    }
}

fn effective(key: &str, stored: PrefValue) -> EffectivePreference {
    // This adapter observes the store reload, not the running consumer's apply acknowledgement.
    // Until such an acknowledgement is wired into this boundary, persistence must not be
    // promoted to application even when another runtime component has an apply leg for the key.
    let applied = false;
    let effective = pf_prefs::spec(key).expect("schema key").default;
    EffectivePreference {
        key: PreferenceKey(key.into()),
        effective: to_port_value(effective),
        stored: to_port_value(stored),
        applied,
    }
}

fn to_port_value(value: PrefValue) -> PreferenceValue {
    match value {
        PrefValue::Bool(value) => PreferenceValue::Bool(value),
        PrefValue::Scalar(value) => PreferenceValue::Integer(value),
        PrefValue::Enum(value) => PreferenceValue::Text(value.into()),
    }
}

fn from_port_value(key: &str, value: PreferenceValue) -> Result<PrefValue, PreferenceError> {
    let candidate = match value {
        PreferenceValue::Bool(value) => PrefValue::Bool(value),
        PreferenceValue::Integer(value) => PrefValue::Scalar(value),
        PreferenceValue::Text(value) => {
            let spec = pf_prefs::spec(key).ok_or(PreferenceError::InvalidValue)?;
            match spec.kind {
                pf_prefs::PrefKind::Enum { variants } => variants
                    .iter()
                    .copied()
                    .find(|variant| *variant == value)
                    .map(PrefValue::Enum)
                    .ok_or(PreferenceError::InvalidValue)?,
                _ => return Err(PreferenceError::InvalidValue),
            }
        }
    };
    pf_prefs::validate(key, candidate).map_err(|_| PreferenceError::InvalidValue)
}

fn map_backend_error(error: PrefError) -> PreferenceError {
    match error {
        PrefError::UnknownKey(_) | PrefError::Type { .. } | PrefError::Range { .. } => {
            PreferenceError::InvalidValue
        }
        PrefError::Io(_) | PrefError::Parse(_) | PrefError::UnsupportedVersion { .. } => {
            PreferenceError::BackendUnavailable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_error_kinds_fail_toward_infrastructure() {
        assert_eq!(
            map_client_set_error(ClientError::Remote {
                message: "schema skew".into(),
                kind: Some(ErrorKind::InvalidValue),
            }),
            PreferenceError::InvalidValue
        );
        for kind in [
            Some(ErrorKind::Store),
            Some(ErrorKind::Internal),
            Some(ErrorKind::Unknown),
            None,
        ] {
            assert_eq!(
                map_client_set_error(ClientError::Remote {
                    message: "daemon failure".into(),
                    kind,
                }),
                PreferenceError::BackendUnavailable
            );
        }
    }
    use pf_ports::{MonotonicTime, PreferencePort};
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pf-prefs-port-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn deadline() -> Deadline {
        Deadline(MonotonicTime::ZERO)
    }

    #[test]
    fn reports_store_values_as_not_applied_without_runtime_acknowledgement() {
        let dir = scratch("truthful");
        let store = PrefsStore::at(&dir);
        store.apply("monoAudio", PrefValue::Bool(true)).unwrap();
        store.apply("reduceMotion", PrefValue::Bool(true)).unwrap();
        let port = PrefsPreferencePort::for_user(store).unwrap();

        let mono = port
            .read(&PreferenceKey("monoAudio".into()))
            .unwrap()
            .unwrap();
        assert_eq!(mono.effective, PreferenceValue::Bool(false));
        assert_eq!(mono.stored, PreferenceValue::Bool(true));
        assert!(!mono.applied);

        let motion = port
            .read(&PreferenceKey("reduceMotion".into()))
            .unwrap()
            .unwrap();
        assert_eq!(motion.effective, PreferenceValue::Bool(false));
        assert_eq!(motion.stored, PreferenceValue::Bool(true));
        assert!(!motion.applied);

        for key in SCHEMA.iter().map(|spec| spec.key) {
            assert!(
                !port
                    .read(&PreferenceKey(key.into()))
                    .unwrap()
                    .unwrap()
                    .applied,
                "{key}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn mono_audio_submission_is_stored_not_applied() {
        let dir = scratch("mono-submit");
        let mut port = PrefsPreferencePort::for_user(PrefsStore::at(&dir)).unwrap();
        let result = port
            .submit_change(PreferenceChange {
                key: PreferenceKey("monoAudio".into()),
                value: PreferenceValue::Bool(true),
                authority: ChangeAuthority(USER_AUTHORITY.into()),
            })
            .unwrap();

        assert_eq!(result, PreferenceChangeResult::StoredNotApplied);
        let mono = port
            .read(&PreferenceKey("monoAudio".into()))
            .unwrap()
            .unwrap();
        assert_eq!(mono.stored, PreferenceValue::Bool(true));
        assert_eq!(mono.effective, PreferenceValue::Bool(false));
        assert!(!mono.applied);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn second_store_handle_write_is_observed_on_poll() {
        let dir = scratch("external");
        let mut port = PrefsPreferencePort::for_user(PrefsStore::at(&dir)).unwrap();
        PrefsStore::at(&dir)
            .apply("highContrast", PrefValue::Bool(true))
            .unwrap();

        let PreferencePoll::Changed(change) = port.next_change(deadline()).unwrap() else {
            panic!("external write was not observed");
        };
        assert_eq!(change.key, PreferenceKey("highContrast".into()));
        assert_eq!(change.stored, PreferenceValue::Bool(true));
        assert_eq!(change.effective, PreferenceValue::Bool(false));
        assert!(!change.applied);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn submission_is_authority_scoped_and_validated() {
        let dir = scratch("authority");
        let mut port = PrefsPreferencePort::for_user(PrefsStore::at(&dir)).unwrap();
        let change = |authority: &str, value: &str| PreferenceChange {
            key: PreferenceKey("textScale".into()),
            value: PreferenceValue::Text(value.into()),
            authority: ChangeAuthority(authority.into()),
        };
        assert_eq!(
            port.submit_change(change("app", "200%")).unwrap(),
            PreferenceChangeResult::Unauthorized
        );
        assert_eq!(
            port.submit_change(change(USER_AUTHORITY, "110%")),
            Err(PreferenceError::InvalidValue)
        );
        assert_eq!(
            port.submit_change(change(USER_AUTHORITY, "200%")).unwrap(),
            PreferenceChangeResult::StoredNotApplied
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
