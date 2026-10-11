//! # pf-menu: menu model and focus engine
//!
//! One data-driven menu vocabulary for Settings, the system menu, pf-osk panels and app
//! menus (design: mission-control `design/input-layer/menu-framework-notes.md` §3, layer 1).
//!
//! - [`model`]: `Menu › Page › Section › Item`, item kinds, bindings, conditions; the JSON form
//!   is platform `schemas/menu-page.schema.json`.
//! - [`kind`]: the frozen item-kind numbering behind the SDK's `PfMenuItemKind`.
//! - [`schema`]: structural validation and bound-value checks against pf-prefs.
//! - [`engine`]: the controller focus engine (reducer from input to effects, no allocation
//!   per input).

pub mod engine;
pub mod kind;
pub mod model;
pub mod schema;
pub mod value;

pub use engine::{
    AnnounceWhat, ApplyResult, ApplyState, EngineOptions, Feedback, ItemIndex, LegendAction,
    LegendEntry, LegendGlyph, LegendLabel, MenuEffect, MenuEngine, MenuInput, Overlay, PageIndex,
    RevertReason,
};
pub use kind::{ItemKindId, KIND_TABLE};
pub use model::{
    Bind, ChoiceOption, Cond, Confirm, DateTimeFields, Item, ItemId, ItemKind, Menu, Msg, Page,
    PageId, Present, Section, TextPurpose, FORMAT_V1,
};
pub use schema::{BindSchemas, MenuError, PrefsSchemas, SchemaLookup, ValueSchema};
pub use value::MenuValue;
