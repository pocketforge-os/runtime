//! The **audio manager** — a device-agnostic routing object (a platform service present on every
//! device). v0 exposes the available sinks and the cooperatively-selected route; real ALSA/PCM
//! routing is the flash→serial HARDWARE GATE's authority. Routing state is stored through the
//! backend's cooperative capability store under `"audio"`, so it observes the SAME value over the
//! in-process and broker backends (the swap holds).
//!
//! ## `monoAudio` (E4) — honored on the routing layer
//!
//! The `monoAudio` accessibility preference is honored HERE, at the routing layer: when the user
//! enables it, [`output_mix`](AudioManager::output_mix) reports [`OutputMix::Mono`], the
//! **sim-visible semantic** an app (or the sim's control surface / a future mixer) reads to render
//! single-channel output. The real on-device DSP/ALSA channel down-mix is post-v0 and
//! hardware-gated — v0 proves the preference is read at the primitive and flips the routing-layer
//! contract, not that silicon mixes channels (R-A honesty). See `docs/PREFERENCES.md`.

use std::sync::Arc;

use crate::backend::Backend;
use crate::error::CapError;

/// A device-agnostic audio output sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSink {
    /// Built-in speaker.
    Speaker,
    /// 3.5 mm / USB-C headphone out.
    Headphone,
}

/// The channel mix the routing layer presents, driven by the `monoAudio` accessibility preference.
/// v0 semantic only (what a cooperative renderer/mixer honors); the real DSP down-mix is
/// hardware-gated (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMix {
    /// Normal stereo output (`monoAudio` off).
    Stereo,
    /// Down-mixed to a single channel for single-ear / hearing accessibility (`monoAudio` on).
    Mono,
}

impl AudioSink {
    /// The stable wire/store token for this sink.
    pub fn as_str(self) -> &'static str {
        match self {
            AudioSink::Speaker => "speaker",
            AudioSink::Headphone => "headphone",
        }
    }

    fn from_str(s: &str) -> Option<AudioSink> {
        match s {
            "speaker" => Some(AudioSink::Speaker),
            "headphone" => Some(AudioSink::Headphone),
            _ => None,
        }
    }
}

/// One device-agnostic audio-routing object.
pub struct AudioManager {
    backend: Arc<dyn Backend>,
}

impl AudioManager {
    /// Build the manager from a session's backend.
    pub fn new(backend: Arc<dyn Backend>) -> AudioManager {
        AudioManager { backend }
    }

    /// The sinks this platform can route to (constant in v0; descriptor-driven later).
    pub fn sinks(&self) -> &'static [AudioSink] {
        &[AudioSink::Speaker, AudioSink::Headphone]
    }

    /// The currently-selected sink (defaults to `Speaker` when unset).
    pub fn current(&self) -> AudioSink {
        match self.backend.get_capability("audio") {
            Ok(v) => AudioSink::from_str(&String::from_utf8_lossy(&v)).unwrap_or(AudioSink::Speaker),
            Err(_) => AudioSink::Speaker,
        }
    }

    /// Route output to `sink` (cooperative; the real mixer change is hardware-gated).
    pub fn route(&self, sink: AudioSink) -> Result<(), CapError> {
        self.backend.set_capability("audio", sink.as_str().as_bytes())
    }

    /// Whether the `monoAudio` accessibility preference (E4) is enabled — read at the primitive
    /// from the backend's preference store.
    pub fn mono_enabled(&self) -> bool {
        self.backend.preference_bool("monoAudio", false)
    }

    /// The effective channel mix the routing layer presents, honoring `monoAudio` (E4). This is
    /// the sim-visible semantic a cooperative renderer/mixer reads; the real DSP down-mix is
    /// post-v0 + hardware-gated (see the module docs).
    pub fn output_mix(&self) -> OutputMix {
        if self.mono_enabled() {
            OutputMix::Mono
        } else {
            OutputMix::Stereo
        }
    }
}
