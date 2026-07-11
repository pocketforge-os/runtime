//! # pf-prefs — the E4 preference data layer
//!
//! A typed **accessibility / user-preference** schema, a persistent JSON store, a validator, a
//! read-API, and a persist-and-signal write seam. This crate is the *data layer* E4 is built on;
//! `.2` wires it into the capability backends (so the facade reads it at the primitive) and
//! attaches the `PrefsDidChange` observer, and `.3` adds the on-panel settings UI that writes it.
//!
//! ## The contract: read-only to apps, cooperatively honored
//!
//! Preferences are **READ-ONLY TO APPS by contract** (owner ruling Q4 / R-A): an app may read a
//! preference (to, say, skip a flashy animation under `reduceMotion`) and — once `.2` lands the
//! observer — subscribe to changes, but it may **never write one**. Authority to change a
//! preference lives with the *user* (the `pf-settings` CLI today; the on-panel settings UI and
//! supervisor later), all going through the single write path here.
//!
//! This contract is **cooperative, permanently** — "contract, cooperatively honored", never an
//! enforcement claim against a hostile app. The v0 facade is an in-process library, so it proves
//! the contract + ergonomics, not confinement. (The *one* path where a preference is enforceable
//! against a non-cooperative app is the FF/rumble route through E2's `uinput`+`EVIOCGRAB` input
//! broker — that R-B nuance is documented where it applies, in `.2`'s integration docs, not
//! here.)
//!
//! ## Not a fork of the capabilities descriptor
//!
//! Preferences are **user-mutable STATE**; hardware **presence** is device-fixed data owned by
//! the E1 capabilities descriptor. This crate never duplicates or forks that descriptor — the
//! two are joined by `device.id` elsewhere, not merged here. `hapticsEnabled == false` and
//! "this a133 has no rumble motor" are deliberately *different* facts that collapse to the *same*
//! silent no-op at the primitive (that unification is `.2`'s job); pf-prefs only owns the
//! preference half.
//!
//! ## Shape at a glance
//!
//! ```no_run
//! use pf_prefs::{PrefsStore, PrefValue};
//!
//! let store = PrefsStore::open_default();          // honors $PF_PREFS_DIR
//! let prefs = store.load().unwrap();               // tolerant: missing file => all defaults
//! if !prefs.haptics_enabled() { /* the primitive no-ops the rumble write */ }
//!
//! // The write authority (CLI / UI) persists-and-signals through one seam:
//! if let Some(change) = store.apply("hapticsEnabled", PrefValue::Bool(false)).unwrap() {
//!     // `.2` fires PrefsDidChange here so a running app reacts live.
//!     let _ = change;
//! }
//! ```

pub mod error;
pub mod prefs;
pub mod schema;
pub mod store;

pub use error::PrefError;
pub use prefs::{PrefChange, Prefs, Source};
pub use schema::{parse_value, spec, validate, PrefKind, PrefSpec, PrefValue, SCHEMA};
pub use store::PrefsStore;
