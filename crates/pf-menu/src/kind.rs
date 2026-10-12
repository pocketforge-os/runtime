//! Frozen item-kind numbering.
//!
//! These numbers are the source of the SDK's `PfMenuItemKind`. They are part of the app ABI
//! once the SDK contract (tsp-ihne1.21) freezes its enums, so a number is never reused or
//! renumbered: new kinds are appended. The SDK `PF_MENU_RANGE` is deliberately pf-menu's
//! [`ItemKindId::Stepper`] (a bounded integer with min, max, step and unit, adjusted with left
//! and right). [`ItemKindId::Slider`] keeps its own number but is system-only in v1.
//!
//! [`FROZEN_V1`] is the literal table; [`check_numbering`] lets another crate (the SDK
//! contract's C header test) prove its constants against it.

use serde::{Deserialize, Serialize};
use std::fmt;

/// The kind of a menu item. `#[repr(u32)]` values are the frozen numbers.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum ItemKindId {
    Action = 0,
    Toggle = 1,
    Choice = 2,
    Stepper = 3,
    Info = 4,
    Slider = 5,
    Text = 6,
    Link = 7,
    Progress = 8,
    Binding = 9,
    DateTime = 10,
    Custom = 11,
}

/// One row of the kind table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KindEntry {
    pub id: ItemKindId,
    /// The JSON `kind` tag in `menu-page.schema.json`.
    pub name: &'static str,
    /// The SDK constant, for the kinds apps may send in v1.
    pub sdk_name: Option<&'static str>,
}

/// Every kind in number order. `KIND_TABLE[n].id as u32 == n`.
pub const KIND_TABLE: &[KindEntry] = &[
    KindEntry {
        id: ItemKindId::Action,
        name: "action",
        sdk_name: Some("PF_MENU_ACTION"),
    },
    KindEntry {
        id: ItemKindId::Toggle,
        name: "toggle",
        sdk_name: Some("PF_MENU_TOGGLE"),
    },
    KindEntry {
        id: ItemKindId::Choice,
        name: "choice",
        sdk_name: Some("PF_MENU_CHOICE"),
    },
    KindEntry {
        id: ItemKindId::Stepper,
        name: "stepper",
        sdk_name: Some("PF_MENU_RANGE"),
    },
    KindEntry {
        id: ItemKindId::Info,
        name: "info",
        sdk_name: Some("PF_MENU_INFO"),
    },
    KindEntry {
        id: ItemKindId::Slider,
        name: "slider",
        sdk_name: None,
    },
    KindEntry {
        id: ItemKindId::Text,
        name: "text",
        sdk_name: None,
    },
    KindEntry {
        id: ItemKindId::Link,
        name: "link",
        sdk_name: None,
    },
    KindEntry {
        id: ItemKindId::Progress,
        name: "progress",
        sdk_name: None,
    },
    KindEntry {
        id: ItemKindId::Binding,
        name: "binding",
        sdk_name: None,
    },
    KindEntry {
        id: ItemKindId::DateTime,
        name: "date_time",
        sdk_name: None,
    },
    KindEntry {
        id: ItemKindId::Custom,
        name: "custom",
        sdk_name: None,
    },
];

/// The frozen v1 numbering as literals: (JSON name, number). Changing an existing row is an
/// ABI break; appending a row is the only allowed edit.
pub const FROZEN_V1: &[(&str, u32)] = &[
    ("action", 0),
    ("toggle", 1),
    ("choice", 2),
    ("stepper", 3),
    ("info", 4),
    ("slider", 5),
    ("text", 6),
    ("link", 7),
    ("progress", 8),
    ("binding", 9),
    ("date_time", 10),
    ("custom", 11),
];

/// The frozen SDK constants: (C name, number). The SDK header must match these exactly.
pub const SDK_V1: &[(&str, u32)] = &[
    ("PF_MENU_ACTION", 0),
    ("PF_MENU_TOGGLE", 1),
    ("PF_MENU_CHOICE", 2),
    ("PF_MENU_RANGE", 3),
    ("PF_MENU_INFO", 4),
];

impl ItemKindId {
    pub const fn number(self) -> u32 {
        self as u32
    }

    pub fn from_number(number: u32) -> Option<Self> {
        KIND_TABLE.get(number as usize).map(|entry| entry.id)
    }

    pub fn from_name(name: &str) -> Option<Self> {
        KIND_TABLE
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| entry.id)
    }

    pub fn name(self) -> &'static str {
        KIND_TABLE[self as usize].name
    }

    /// True for kinds an app may declare (SDK tier A). Custom is never app-visible.
    pub fn app_visible(self) -> bool {
        KIND_TABLE[self as usize].sdk_name.is_some()
    }
}

impl fmt::Display for ItemKindId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Check a consumer's (name, number) constants against the frozen tables. Accepts either JSON
/// names (`"stepper"`) or SDK names (`"PF_MENU_RANGE"`). Every row given must match; a
/// consumer may list fewer kinds than exist, never a different number.
pub fn check_numbering(constants: &[(&str, u32)]) -> Result<(), String> {
    for &(name, number) in constants {
        let expected = FROZEN_V1
            .iter()
            .chain(SDK_V1.iter())
            .find(|(frozen, _)| *frozen == name)
            .map(|&(_, n)| n)
            .ok_or_else(|| format!("{name} is not a frozen pf-menu kind"))?;
        if expected != number {
            return Err(format!("{name} is {number}, frozen value is {expected}"));
        }
    }
    Ok(())
}
