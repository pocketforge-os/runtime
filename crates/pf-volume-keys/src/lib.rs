//! Volume-key handling and the system volume toast (tsp-ihne1.63).
//!
//! [`VolumeKeys`] is the pure state machine hosted by the input broker: key press/release and its
//! own 400 ms / 100 ms auto-repeat in, one preference write per step out, and a [`ToastModel`]
//! for the OSD. [`toast`] turns that model into a `pf-scene` scene that `pf-render` rasterizes
//! into a buffer; no surface or protocol is involved, so the trusted-overlay binding
//! (tsp-ihne1.63.1) can attach later.

mod engine;
pub mod host;
pub mod toast;

pub use engine::{
    Key, Outcome, PrefStore, PrefsdStore, VolumeKeys, VolumeState, REPEAT_DELAY, REPEAT_INTERVAL,
    STEP,
};
pub use toast::ToastModel;
