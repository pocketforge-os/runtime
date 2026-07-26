//! # pf-collect-ui — the guided-collection on-panel UI (tsp-bwrg.3)
//!
//! The on-device front end for the headless guided-collection engine
//! ([`pf_input_collect`], tsp-bwrg.2). It renders the REAL DEVICE on the device's own screen,
//! highlights the control the engine is prompting for, shows the prompt + progress, and drives the
//! engine's `Collector` state machine from on-panel input.
//!
//! ## Render host (architecture decision, tsp-bwrg.3; owner-directed pivot 2026-07-26)
//! The FACE is the real device's 3D model, CONSUMED from the platform `.scad`->skin chain's
//! committed, drift-gated (tsp-65jc.7) assets — NOT hand-drawn, NOT re-generated here. This app is
//! a runtime READ path (the first consumer of `[skin.views.*]`): it draws the neutral `body` and
//! blits the active control's crop from the `lit` atlas over its `[skin.parts]` rect (the
//! pf-hwprobe highlight model), using the `[skin.views.top]` atlas for the top-edge shoulders /
//! triggers. See [`skin`].
//!
//! The renderer is a **pure-fbdev** software path ([`fbdev::FbDev`]) with a backend-agnostic
//! [`canvas::Canvas`] the SAME draw code fills whether the sink is `/dev/fb0` on-panel or a PPM
//! ([`dump`]) off-device — so the render is validated headless before the device is touched.
//! libc + pure-Rust decode (png/toml/serde) only → a single static aarch64-musl binary that runs
//! during bring-up before the GPU stack and sidesteps tsp-osr (an SDL-on-sunxifb issue). Full
//! rationale: `docs/RENDER_HOST_DECISION.md`.

pub mod canvas;
pub mod dump;
pub mod fbdev;
pub mod font;
pub mod image;
pub mod render;
pub mod skin;
pub mod wizard;
