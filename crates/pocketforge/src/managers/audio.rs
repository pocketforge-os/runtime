//! The **audio manager** — a device-agnostic routing object (a platform service present on every
//! device). v0 exposes the available sinks and the cooperatively-selected route; real ALSA/PCM
//! routing is the flash→serial HARDWARE GATE's authority. Routing state is stored through the
//! backend's cooperative capability store under `"audio"`, so it observes the SAME value over the
//! in-process and broker backends (the swap holds).

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
}
