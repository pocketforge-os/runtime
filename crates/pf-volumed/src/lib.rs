//! System-volume state machine and the current ALSA backend.
//!
//! Preference and route policy deliberately depend on [`MixerBackend`], not ALSA.  PipeWire /
//! WirePlumber can therefore replace [`AlsaMixer`] without changing the per-output preference
//! contract when tsp-ihne1.30 lands.

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

    fn apply(&mut self, output: Output, percent: u8, muted: bool) -> Result<(), Self::Error>;
}

pub struct VolumeService<M> {
    backend: M,
    active: Output,
    preferences: VolumePreferences,
}

impl<M: MixerBackend> VolumeService<M> {
    pub fn new(backend: M, headphones_plugged: bool, preferences: VolumePreferences) -> Self {
        Self {
            backend,
            active: output_for_plug(headphones_plugged),
            preferences,
        }
    }

    pub fn apply_current(&mut self) -> Result<(), M::Error> {
        self.backend.apply(
            self.active,
            self.preferences.percent(self.active),
            self.preferences.muted(),
        )
    }

    pub fn preferences_changed(&mut self, preferences: VolumePreferences) -> Result<(), M::Error> {
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

    pub fn plug_changed(&mut self, headphones_plugged: bool) -> Result<(), M::Error> {
        let active = output_for_plug(headphones_plugged);
        if active == self.active {
            return Ok(());
        }
        self.backend.apply(
            active,
            self.preferences.percent(active),
            self.preferences.muted(),
        )?;
        self.active = active;
        Ok(())
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

fn output_for_plug(headphones_plugged: bool) -> Output {
    if headphones_plugged {
        Output::Headphones
    } else {
        Output::Speaker
    }
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

/// Current hardware backend. Both physical endpoints share the codec's DAC playback level; the
/// kernel jack/DAPM policy selects `Headphone` versus `Speaker`. `output` remains in the trait so
/// a future WirePlumber backend can address distinct sinks.
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

    fn apply(&mut self, _output: Output, percent: u8, muted: bool) -> Result<(), Self::Error> {
        let raw = ((u16::from(percent) * 63 + 50) / 100).to_string();
        self.set_control("DAC Playback Volume", &raw)?;
        self.set_control("DAC Playback Switch", if muted { "off" } else { "on" })
    }
}

pub struct AlsaJackSource {
    amixer: PathBuf,
    card: String,
}

impl AlsaJackSource {
    pub fn new(card: impl Into<String>) -> Self {
        Self::with_amixer("/usr/bin/amixer", card)
    }

    pub fn with_amixer(path: impl Into<PathBuf>, card: impl Into<String>) -> Self {
        Self {
            amixer: path.into(),
            card: card.into(),
        }
    }

    pub fn headphones_plugged(&self) -> Result<bool, RuntimeError> {
        let output = Command::new(&self.amixer)
            .args(["-c", &self.card, "cget", "name=Headphone Jack"])
            .output()
            .map_err(|error| RuntimeError(format!("start {}: {error}", self.amixer.display())))?;
        if !output.status.success() {
            return Err(RuntimeError::command("read ALSA Headphone Jack", output));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        match (stdout.contains("values=on"), stdout.contains("values=off")) {
            (true, false) => Ok(true),
            (false, true) => Ok(false),
            _ => Err(RuntimeError(format!(
                "ALSA Headphone Jack response has no unique on/off value: {stdout:?}"
            ))),
        }
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
