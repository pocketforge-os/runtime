use crate::toast::ToastModel;
use pf_prefsd::Client;
use pf_volumed::jack::RouteSource;
use pf_volumed::{Output, HEADPHONES_KEY, MUTED_KEY, SPEAKER_KEY};
use serde_json::Value;
use std::time::Duration;

/// Percent per press (every-menu-spec 7.1).
pub const STEP: u8 = 5;
pub const REPEAT_DELAY: Duration = Duration::from_millis(400);
pub const REPEAT_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VolumeState {
    pub speaker: u8,
    pub headphones: u8,
    pub muted: bool,
}

/// Preference access. The production implementation is [`PrefsdStore`]; tests use a fake.
pub trait PrefStore {
    fn load(&mut self) -> Result<VolumeState, String>;
    fn store(&mut self, key: &str, value: Value) -> Result<(), String>;
}

/// pf-prefsd-backed store; writes are authorised by the peer's systemd unit, not by this code.
pub struct PrefsdStore(pub Client);

impl PrefStore for PrefsdStore {
    fn load(&mut self) -> Result<VolumeState, String> {
        let values = self.0.get_all().map_err(|error| error.to_string())?;
        let level = |key: &str| {
            values
                .get(key)
                .and_then(Value::as_u64)
                .filter(|value| *value <= 100)
                .map(|value| value as u8)
                .ok_or_else(|| format!("preference {key} unreadable"))
        };
        Ok(VolumeState {
            speaker: level(SPEAKER_KEY)?,
            headphones: level(HEADPHONES_KEY)?,
            muted: values
                .get(MUTED_KEY)
                .and_then(Value::as_bool)
                .ok_or_else(|| format!("preference {MUTED_KEY} unreadable"))?,
        })
    }

    fn store(&mut self, key: &str, value: Value) -> Result<(), String> {
        self.0
            .set(key, value)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Shown(ToastModel),
}

struct Held {
    key: Key,
    next: Duration,
}

pub struct VolumeKeys<S, R> {
    store: S,
    route: R,
    held: Option<Held>,
}

impl<S: PrefStore, R: RouteSource> VolumeKeys<S, R> {
    pub fn new(store: S, route: R) -> Self {
        Self {
            store,
            route,
            held: None,
        }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    /// A key went down at `now`. Kernel autorepeat (a second down for the held key) is ignored:
    /// this handler owns the repeat schedule so it is identical on every input device.
    pub fn press(&mut self, key: Key, now: Duration) -> Option<Outcome> {
        if self.held.as_ref().is_some_and(|held| held.key == key) {
            return None;
        }
        self.held = Some(Held {
            key,
            next: now + REPEAT_DELAY,
        });
        Some(self.step(key))
    }

    pub fn release(&mut self, key: Key) {
        if self.held.as_ref().is_some_and(|held| held.key == key) {
            self.held = None;
        }
    }

    /// Emit every repeat step due at `now`; a late tick catches up instead of dropping steps.
    pub fn tick(&mut self, now: Duration) -> Vec<Outcome> {
        let mut out = Vec::new();
        while let Some(held) = self.held.as_mut().filter(|held| held.next <= now) {
            let key = held.key;
            held.next += REPEAT_INTERVAL;
            out.push(self.step(key));
        }
        out
    }

    fn step(&mut self, key: Key) -> Outcome {
        match self.try_step(key) {
            Ok(model) => Outcome::Shown(model),
            Err(message) => Outcome::Shown(ToastModel::Error { message }),
        }
    }

    fn try_step(&mut self, key: Key) -> Result<ToastModel, String> {
        let output = self
            .route
            .route()
            .map_err(|error| format!("route unknown: {error}"))?;
        let state = self.store.load()?;
        let (name, current) = match output {
            Output::Speaker => (SPEAKER_KEY, state.speaker),
            Output::Headphones => (HEADPHONES_KEY, state.headphones),
        };
        let next = match key {
            Key::Up => current.saturating_add(STEP).min(100),
            Key::Down => current.saturating_sub(STEP),
        };
        if next != current {
            self.store.store(name, Value::from(next))?;
        }
        // Level first, unmute last: the new level is stored before it can be heard.
        if state.muted {
            self.store.store(MUTED_KEY, Value::Bool(false))?;
        }
        Ok(ToastModel::Level {
            percent: next,
            output,
            muted: false,
        })
    }
}
