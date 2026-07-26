//! # pf-collect-ui — the guided-collection on-panel UI (tsp-bwrg.3)
//!
//! The on-device front end for the headless guided-collection engine
//! ([`pf_input_collect`], tsp-bwrg.2). It renders a canonical gamepad FACE on the device's own
//! screen, highlights the control the engine is prompting for, shows the prompt + progress, and
//! drives the engine's `Collector` state machine from on-panel input.
//!
//! ## Render host (architecture decision, tsp-bwrg.3)
//! A NEW minimal renderer (NOT pf-hwprobe reuse — that is the descriptor-driven VERIFY UI, a
//! sim-harness slave, wrong shape for an autonomous prompt sequence with no descriptor). It is a
//! **pure-fbdev software renderer**: a backend-agnostic [`canvas::Canvas`] is filled by the SAME
//! draw code whether the sink is [`fbdev::FbDev`] (`/dev/fb0`, on-panel) or a PPM file
//! ([`dump`], off-device validation). libc-only, static aarch64-musl, single OTA binary — so it
//! runs during bring-up *before* the GPU stack and sidesteps tsp-osr (which is an SDL-on-sunxifb
//! issue: on sunxifb every SDL renderer is the PowerVR GLES2 path, which a bring-up tool cannot
//! depend on). SDL3 + the tsp-osr recipe-B is the documented fb-down fallback — see
//! `docs/RENDER_HOST_DECISION.md`.

pub mod canvas;
pub mod dump;
pub mod face;
pub mod fbdev;
pub mod font;
pub mod render;
pub mod wizard;
