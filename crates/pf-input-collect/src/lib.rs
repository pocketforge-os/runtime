//! # `pf-input-collect` — the guided-collection ENGINE (`tsp-bwrg.2`)
//!
//! The headless data heart of the input bring-up / calibration app (epic `tsp-bwrg`, job #2:
//! guided ground-truth collection). With **no pre-filled descriptor** it prompts for each control
//! in turn, reads the raw `/dev/input/eventN` node DIRECTLY (post-decoder), records the raw evdev
//! event(s) per prompt, classifies binary-vs-analog triggers from OBSERVED behaviour, and emits a
//! candidate [`emit::Capabilities`] (`capabilities.toml`) for a brand-new device.
//!
//! ## Why it can't go through `pf-input-broker`
//!
//! `pf_input_broker::InputBroker::start_with()` requires a `Descriptor` to construct — the remap
//! action-map, the uinput spec. Guided collection is the mode that RUNS BEFORE a descriptor
//! exists; it has nothing to build a broker from. So it is a passive reader of the raw node,
//! exactly as `platform/regression/caps/evdev-probe.py --watch` is. The evdev ioctl primitives
//! it needs (`EVIOCGNAME`/`EVIOCGID`/`EVIOCGABS` + `poll`/`read`) are carried self-contained in
//! [`source`] — typed as `libc::Ioctl`, the `pf-hw-exerciser` precedent — so the device binary
//! cross-compiles to `aarch64-unknown-linux-musl` (static) and stays decoupled from in-flight
//! edits to `pf-input-broker`.
//!
//! ## The two consumers
//!
//! - The **on-panel UI** (`tsp-bwrg.3`) drives the engine programmatically: it renders the device
//!   face, calls [`Collector::prompt`] to show the ask, pumps its own events into
//!   [`Collector::record`], then [`Collector::commit_current`] / [`Collector::advance`] /
//!   [`Collector::back`], and finally [`Collector::emit`].
//! - The **CLI** (`pf-input-collect`) drives it headlessly via [`collect::run`] over an
//!   [`source::EvdevSource`].
//!
//! Both paths are identical logic; the unit tests drive the SAME [`collect::run`] over a
//! [`source::ScriptedSource`] (a synthetic a133-shaped stream — no kernel, no `/dev/uinput`).

pub mod codes;
pub mod collect;
pub mod emit;
pub mod plan;
pub mod source;

pub use collect::{Collector, CollectError, CommitOutcome, DeviceMeta, Recorded, RunConfig, Semantics};
pub use emit::Capabilities;
pub use plan::{default_gamepad_plan, ControlSpec, Kind};
pub use source::{AbsInfo, EventSource, EvdevSource, Identity, RawEvent, ScriptedSource};
