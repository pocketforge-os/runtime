//! System-volume state machine and the current ALSA backend.
//!
//! Preference and route policy deliberately depend on [`MixerBackend`], not ALSA.  PipeWire /
//! WirePlumber can therefore replace [`AlsaMixer`] without changing the per-output preference
//! contract when tsp-ihne1.30 lands.
//!
//! The physical route comes from the codec's headphone-jack input switch ([`jack`]); the kernel's
//! codec/DAPM policy selects the analog path, so [`AlsaMixer`]'s transition quiesces the shared
//! DAC and then applies the destination output's stored level. When the route is unknown (switch
//! device absent or unreadable) the service holds the DAC at its minimum.

pub mod jack;

use jack::RouteSource;
use pf_prefsd::Client;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::process::{Command, Output as CommandOutput};

pub const SPEAKER_KEY: &str = "volumeSpeaker";
pub const HEADPHONES_KEY: &str = "volumeHeadphones";
pub const MUTED_KEY: &str = "muted";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Output {
    Speaker,
    Headphones,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VolumePreferences {
    speaker: u8,
    headphones: u8,
    muted: bool,
}

impl VolumePreferences {
    pub fn new(speaker: u8, headphones: u8, muted: bool) -> Result<Self, PreferenceError> {
        if speaker > 100 {
            return Err(PreferenceError::OutOfRange(SPEAKER_KEY));
        }
        if headphones > 100 {
            return Err(PreferenceError::OutOfRange(HEADPHONES_KEY));
        }
        Ok(Self {
            speaker,
            headphones,
            muted,
        })
    }

    pub fn from_values(values: &BTreeMap<String, Value>) -> Result<Self, PreferenceError> {
        let scalar = |key: &'static str| -> Result<u8, PreferenceError> {
            let raw = values
                .get(key)
                .and_then(Value::as_u64)
                .ok_or(PreferenceError::WrongType(key))?;
            u8::try_from(raw)
                .ok()
                .filter(|value| *value <= 100)
                .ok_or(PreferenceError::OutOfRange(key))
        };
        let muted = values
            .get(MUTED_KEY)
            .and_then(Value::as_bool)
            .ok_or(PreferenceError::WrongType(MUTED_KEY))?;
        Self::new(scalar(SPEAKER_KEY)?, scalar(HEADPHONES_KEY)?, muted)
    }

    pub fn percent(self, output: Output) -> u8 {
        match output {
            Output::Speaker => self.speaker,
            Output::Headphones => self.headphones,
        }
    }

    pub fn muted(self) -> bool {
        self.muted
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreferenceError {
    WrongType(&'static str),
    OutOfRange(&'static str),
}

impl fmt::Display for PreferenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongType(key) => write!(formatter, "preference {key} has the wrong type"),
            Self::OutOfRange(key) => write!(formatter, "preference {key} is outside 0..=100"),
        }
    }
}

impl std::error::Error for PreferenceError {}

pub trait MixerBackend {
    type Error;

    /// Put the shared volume path at its hardware minimum before route ownership is known.
    fn quiesce(&mut self) -> Result<(), Self::Error>;

    fn apply(&mut self, output: Output, percent: u8, muted: bool) -> Result<(), Self::Error>;

    /// Select `output` without exposing it at the previous route's level.
    ///
    /// Implementations own the whole quiesce/select/apply transaction. On error they must either
    /// leave the previously selected route intact or leave the volume path quiesced. The caller
    /// treats the physical route as unknown until a later transition succeeds.
    fn transition(&mut self, output: Output, percent: u8, muted: bool) -> Result<(), Self::Error>;
}

pub struct VolumeService<M> {
    backend: M,
    active: Output,
    preferences: VolumePreferences,
    route_uncertain: bool,
}

impl<M: MixerBackend> VolumeService<M> {
    pub fn new(backend: M, active: Output, preferences: VolumePreferences) -> Self {
        Self {
            backend,
            active,
            preferences,
            route_uncertain: false,
        }
    }

    /// A service whose physical route has not been established: the first `reconcile` performs a
    /// full transition, so no level is exposed before the route is known.
    pub fn unrouted(backend: M, preferences: VolumePreferences) -> Self {
        Self {
            backend,
            active: Output::Speaker,
            preferences,
            route_uncertain: true,
        }
    }

    /// The route became unknown: hold the volume path at its minimum until a transition succeeds.
    pub fn route_lost(&mut self) -> Result<(), M::Error> {
        self.route_uncertain = true;
        self.backend.quiesce()
    }

    pub fn apply_current(&mut self) -> Result<(), M::Error> {
        if self.route_uncertain {
            self.backend.transition(
                self.active,
                self.preferences.percent(self.active),
                self.preferences.muted(),
            )?;
            self.route_uncertain = false;
            Ok(())
        } else {
            self.backend.apply(
                self.active,
                self.preferences.percent(self.active),
                self.preferences.muted(),
            )
        }
    }

    /// Reconcile one serialized route/preference observation.
    ///
    /// Combining the observations prevents preference-first handling from applying a source
    /// level just before a route change. A failed transition does not commit either observation;
    /// the next call performs another safe transition, even when it requests the old route.
    pub fn reconcile(
        &mut self,
        active: Output,
        preferences: VolumePreferences,
    ) -> Result<(), M::Error> {
        if active != self.active || self.route_uncertain {
            if let Err(error) =
                self.backend
                    .transition(active, preferences.percent(active), preferences.muted())
            {
                self.route_uncertain = true;
                return Err(error);
            }
            self.active = active;
            self.preferences = preferences;
            self.route_uncertain = false;
            return Ok(());
        }

        if preferences == self.preferences {
            return Ok(());
        }
        self.backend.apply(
            self.active,
            preferences.percent(self.active),
            preferences.muted(),
        )?;
        self.preferences = preferences;
        Ok(())
    }

    /// Read the route and reconcile it with the latest preferences. An unreadable route
    /// quiesces the volume path instead of keeping the last level.
    pub fn sync_route<S: RouteSource>(
        &mut self,
        source: &mut S,
        preferences: VolumePreferences,
    ) -> Result<(), SyncError<M::Error>> {
        match source.route() {
            Ok(route) => self.reconcile(route, preferences).map_err(SyncError::Mixer),
            Err(error) => {
                self.route_lost().map_err(SyncError::Mixer)?;
                Err(SyncError::Route(error))
            }
        }
    }

    pub fn active_output(&self) -> Output {
        self.active
    }

    pub fn backend(&self) -> &M {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut M {
        &mut self.backend
    }
}

#[derive(Debug)]
pub enum SyncError<E> {
    /// The route source failed; the volume path was quiesced.
    Route(jack::RouteError),
    Mixer(E),
}

#[derive(Debug)]
pub struct RuntimeError(String);

impl RuntimeError {
    fn command(operation: &str, output: CommandOutput) -> Self {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        Self(format!(
            "{operation} failed (status {}): stderr={:?} stdout={:?}",
            output.status, stderr, stdout
        ))
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RuntimeError {}

/// Current hardware backend. Both physical endpoints share the codec's DAC playback level. This
/// board's route comes from the jack input switch ([`jack`]); the kernel selects the analog path
/// and this backend orders the level changes. A future WirePlumber backend can own the route.
pub struct AlsaMixer {
    amixer: PathBuf,
    card: String,
}

impl AlsaMixer {
    pub fn new(card: impl Into<String>) -> Self {
        Self::with_amixer("/usr/bin/amixer", card)
    }

    pub fn with_amixer(path: impl Into<PathBuf>, card: impl Into<String>) -> Self {
        Self {
            amixer: path.into(),
            card: card.into(),
        }
    }

    fn set_control(&self, name: &str, value: &str) -> Result<(), RuntimeError> {
        let output = Command::new(&self.amixer)
            .args([
                "-q",
                "-c",
                &self.card,
                "cset",
                &format!("name={name}"),
                value,
            ])
            .output()
            .map_err(|error| RuntimeError(format!("start {}: {error}", self.amixer.display())))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(RuntimeError::command(
                &format!("set ALSA control {name}"),
                output,
            ))
        }
    }
}

impl MixerBackend for AlsaMixer {
    type Error = RuntimeError;

    fn quiesce(&mut self) -> Result<(), Self::Error> {
        self.set_control("DAC Playback Volume", "0")
    }

    fn apply(&mut self, _output: Output, percent: u8, muted: bool) -> Result<(), Self::Error> {
        // The merged A133 driver exposes no independent DAC mute switch. Its 0..63 inverted
        // volume control reaches -74.24 dB at 0, so mute applies that minimum while the service
        // retains the per-output percentage for the unmute round-trip.
        let raw = if muted {
            0
        } else {
            (u16::from(percent) * 63 + 50) / 100
        };
        self.set_control("DAC Playback Volume", &raw.to_string())
    }

    fn transition(&mut self, output: Output, percent: u8, muted: bool) -> Result<(), Self::Error> {
        // The codec's jack/DAPM policy selects the analog path (see mixer-defaults); this backend
        // owns the level ordering: minimum first, then the destination output's stored level.
        self.quiesce()?;
        self.apply(output, percent, muted)
    }
}

pub fn load_preferences(client: &Client) -> Result<VolumePreferences, RuntimeError> {
    let values = client
        .get_all()
        .map_err(|error| RuntimeError(format!("read system preferences: {error}")))?;
    VolumePreferences::from_values(&values)
        .map_err(|error| RuntimeError(format!("read system preferences: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_require_all_three_typed_keys() {
        let good = BTreeMap::from([
            (SPEAKER_KEY.into(), Value::from(60)),
            (HEADPHONES_KEY.into(), Value::from(40)),
            (MUTED_KEY.into(), Value::from(false)),
        ]);
        assert_eq!(
            VolumePreferences::from_values(&good),
            VolumePreferences::new(60, 40, false)
        );

        let mut bad = good;
        bad.insert(MUTED_KEY.into(), Value::from(0));
        assert_eq!(
            VolumePreferences::from_values(&bad),
            Err(PreferenceError::WrongType(MUTED_KEY))
        );
    }
}
