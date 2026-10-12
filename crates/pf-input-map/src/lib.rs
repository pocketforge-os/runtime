//! Effective input maps, transactional remapping, and glyph resolution.
//!
//! The crate consumes the frozen `shell-input-contract` JSON shape (schema versions 2 and
//! 3; v3 adds the device classes plus the osk/osk.edit/pointer contexts). It owns policy
//! and persistence, but deliberately does not read input devices or render glyphs.
//!
//! Persistence (X16): [`SystemRemapStore`] serves the router-owned system document at
//! [`SYSTEM_REMAP_PATH`] and migrates the legacy shell-state `remaps.json` once, in memory,
//! without ever mutating the legacy file. See docs/INPUT-MAPPING.md for the X19 precedence
//! rule that consumer SDKs apply on top of these maps.

use pf_ports::{EffectiveBinding, GlyphError, GlyphResolver, GlyphResult, ShellAction};
use pf_scene::AxisMove;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// Version of the persisted remap document (X16 router-owned store). Contract versions are
/// tracked separately: the crate consumes frozen shell-input-contract v2 and v3 documents.
const SCHEMA_VERSION: u32 = 2;
const CONTRACT_VERSIONS: [u32; 2] = [2, 3];
const PROTECTED: [&str; 4] = ["Activate", "Back", "SystemMenu", "Start"];
/// Reachability is keyed by (context, action) pair: contract v3 legitimately ships `Back` in
/// shell, osk.edit and pointer at once, and only the shell pair is the protected one.
const PROTECTED_PAIRS: [(&str, &str); 4] = [
    ("shell", "Activate"),
    ("shell", "Back"),
    ("global", "SystemMenu"),
    ("global", "Start"),
];
const FACE_ACTIONS: [&str; 6] = [
    "Activate",
    "Back",
    "Quick",
    "Search.open",
    "Search.submit",
    "Search.cancel",
];
const ACTIONS: [&str; 42] = [
    "Activate",
    "Back",
    "Move.up",
    "Move.down",
    "Move.left",
    "Move.right",
    "Quick",
    "Search.open",
    "Search.submit",
    "Search.cancel",
    "SystemMenu",
    "Start",
    // osk (spec §4.1)
    "Type",
    "Close",
    "Backspace",
    "Space",
    "Shift",
    "Symbols",
    "CursorLeft",
    "CursorRight",
    "Commit",
    "NextLayout",
    "EditPad",
    "SuggestPick",
    "SuggestList",
    // osk.edit (spec §4.1); `Back` is shared with shell and pointer and listed once.
    "WordLeft",
    "WordRight",
    "Select",
    "SelectAll",
    "Copy",
    "Cut",
    "Paste",
    "Done",
    // pointer (spec §4.3.1)
    "LeftClick",
    "RightClick",
    "MiddleClick",
    "Escape",
    "Forward",
    "Slow",
    "ScrollMode",
    "Enter",
    "Tab",
];
const POSITIONS: [&str; 20] = [
    "east",
    "south",
    "west",
    "north",
    "start",
    "select",
    "guide",
    "home",
    "l1",
    "r1",
    "l2",
    "r2",
    "l3",
    "r3",
    "dpad_up",
    "dpad_down",
    "dpad_left",
    "dpad_right",
    "lstick",
    "rstick",
];
const STICK_POSITIONS: [&str; 2] = ["lstick", "rstick"];
/// The frozen v2 vocabulary. A v2 contract must reject every v3-only construct: a document
/// is interpreted with exactly the semantics its version negotiated.
const V2_POSITIONS: [&str; 12] = [
    "east", "south", "west", "north", "start", "select", "guide", "home", "l1", "r1", "l2", "r2",
];
const V2_CONTEXTS: [&str; 3] = ["shell", "search", "global"];
const V2_ACTIONS: [&str; 12] = [
    "Activate",
    "Back",
    "Move.up",
    "Move.down",
    "Move.left",
    "Move.right",
    "Quick",
    "Search.open",
    "Search.submit",
    "Search.cancel",
    "SystemMenu",
    "Start",
];
const DEVICE_CLASSES: [&str; 3] = ["two_stick", "no_stick", "minimal"];
/// Every context the contract knows (X15). `osk.num` inherits from `osk` and never maps.
const CONTEXTS: [&str; 7] = [
    "shell", "search", "global", "osk", "osk.edit", "osk.num", "pointer",
];
const KEYBOARD_CONTEXTS: [&str; 2] = ["osk", "osk.edit"];
const OSK_NUM_INHERITED: [&str; 4] = ["Type", "Close", "Backspace", "Commit"];
/// Rule P2 (spec §4.2): these keyboard actions must keep a physical binding; an on-screen-only
/// resolution is refused.
const REQUIRED_PHYSICAL_KEYBOARD: [(&str, &str); 5] = [
    ("osk", "Type"),
    ("osk", "Close"),
    ("osk", "Backspace"),
    ("osk", "Space"),
    ("osk", "SuggestPick"),
];
/// Glyph lookup order when the same action name exists in several contexts (e.g. `Back`):
/// the shell's own contexts win over keyboard and pointer ones.
const GLYPH_CONTEXT_PRIORITY: [&str; 6] =
    ["shell", "global", "search", "osk", "osk.edit", "pointer"];

/// Every context the crate knows (X15). `osk.num` inherits from `osk` and never maps.
pub fn known_contexts() -> &'static [&'static str] {
    &CONTEXTS
}

/// The canonical contexts each action may be mapped in. Mirrors the platform contract
/// validator's ACTION_CONTEXTS table.
fn allowed_contexts(action: &str) -> Option<&'static [&'static str]> {
    Some(match action {
        "Activate" | "Move.up" | "Move.down" | "Move.left" | "Move.right" | "Quick" => &["shell"],
        // `Back` is the one action shared by three contexts.
        "Back" => &["shell", "osk.edit", "pointer"],
        "Search.open" | "Search.submit" | "Search.cancel" => &["search"],
        "SystemMenu" | "Start" => &["global"],
        "Type" | "Close" | "Backspace" | "Space" | "Shift" | "Symbols" | "CursorLeft"
        | "CursorRight" | "Commit" | "NextLayout" | "EditPad" | "SuggestPick" | "SuggestList" => {
            &["osk"]
        }
        "WordLeft" | "WordRight" | "Select" | "SelectAll" | "Copy" | "Cut" | "Paste" | "Done" => {
            &["osk.edit"]
        }
        "LeftClick" | "RightClick" | "MiddleClick" | "Escape" | "Forward" | "Slow"
        | "ScrollMode" | "Enter" | "Tab" => &["pointer"],
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    Horizontal,
    Vertical,
}

impl Axis {
    fn as_str(self) -> &'static str {
        match self {
            Self::Horizontal => "horizontal",
            Self::Vertical => "vertical",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Left => "left",
            Self::Right => "right",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingShape {
    SinglePress,
    Chord,
    DoublePress,
    Hold,
    /// Contract v3: a stick deflection along one axis (suggestion pick).
    StickAxis,
    /// Contract v3: a stick deflection in one direction (suggestion list).
    StickDirection,
    /// Contract v3: resolved by an on-screen key in the keyboard contexts; no physical control.
    OnScreen,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub shape: BindingShape,
    #[serde(default)]
    pub controls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_interval_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_duration_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub axis: Option<Axis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<Direction>,
}

/// Deterministic comparison form of a binding (shape, sorted controls, timings, axis, direction).
type BindingSignature<'a> = (
    BindingShape,
    Vec<&'a str>,
    Option<u32>,
    Option<u32>,
    Option<Axis>,
    Option<Direction>,
);

// Ordering is needed for deterministic signatures, while BindingShape's source order is policy-free.
impl Ord for BindingShape {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank().cmp(&other.rank())
    }
}
impl PartialOrd for BindingShape {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl BindingShape {
    fn rank(&self) -> u8 {
        match self {
            Self::SinglePress => 0,
            Self::Chord => 1,
            Self::DoublePress => 2,
            Self::Hold => 3,
            Self::StickAxis => 4,
            Self::StickDirection => 5,
            Self::OnScreen => 6,
        }
    }
}

impl Binding {
    pub fn single(control: impl Into<String>) -> Self {
        Self {
            shape: BindingShape::SinglePress,
            controls: vec![control.into()],
            max_interval_ms: None,
            min_duration_ms: None,
            axis: None,
            direction: None,
        }
    }

    pub fn hold(control: impl Into<String>, min_duration_ms: u32) -> Self {
        Self {
            shape: BindingShape::Hold,
            controls: vec![control.into()],
            max_interval_ms: None,
            min_duration_ms: Some(min_duration_ms),
            axis: None,
            direction: None,
        }
    }

    pub fn stick_axis(control: impl Into<String>, axis: Axis) -> Self {
        Self {
            shape: BindingShape::StickAxis,
            controls: vec![control.into()],
            max_interval_ms: None,
            min_duration_ms: None,
            axis: Some(axis),
            direction: None,
        }
    }

    pub fn stick_direction(control: impl Into<String>, direction: Direction) -> Self {
        Self {
            shape: BindingShape::StickDirection,
            controls: vec![control.into()],
            max_interval_ms: None,
            min_duration_ms: None,
            axis: None,
            direction: Some(direction),
        }
    }

    pub fn on_screen() -> Self {
        Self {
            shape: BindingShape::OnScreen,
            controls: Vec::new(),
            max_interval_ms: None,
            min_duration_ms: None,
            axis: None,
            direction: None,
        }
    }

    fn validate(&self) -> Result<(), MapError> {
        if self.controls.iter().any(String::is_empty) {
            return Err(MapError::InvalidBinding(
                "controls must be non-empty".into(),
            ));
        }
        let unique: BTreeSet<_> = self.controls.iter().collect();
        if unique.len() != self.controls.len() {
            return Err(MapError::InvalidBinding("controls must be unique".into()));
        }
        match self.shape {
            BindingShape::OnScreen => {
                if !self.controls.is_empty() {
                    return Err(MapError::InvalidBinding(
                        "on_screen takes no controls".into(),
                    ));
                }
                if self.max_interval_ms.is_some()
                    || self.min_duration_ms.is_some()
                    || self.axis.is_some()
                    || self.direction.is_some()
                {
                    return Err(MapError::InvalidBinding(
                        "on_screen takes no timing, axis or direction fields".into(),
                    ));
                }
                Ok(())
            }
            BindingShape::StickAxis | BindingShape::StickDirection => {
                if self.controls.len() != 1 || !STICK_POSITIONS.contains(&self.controls[0].as_str())
                {
                    return Err(MapError::InvalidBinding(
                        "stick shapes require exactly one of lstick or rstick".into(),
                    ));
                }
                if self.max_interval_ms.is_some() || self.min_duration_ms.is_some() {
                    return Err(MapError::InvalidBinding(
                        "timing field is invalid for stick shapes".into(),
                    ));
                }
                match self.shape {
                    BindingShape::StickAxis => {
                        if self.axis.is_none() || self.direction.is_some() {
                            return Err(MapError::InvalidBinding(
                                "stick_axis requires an axis and no direction".into(),
                            ));
                        }
                    }
                    BindingShape::StickDirection => {
                        if self.direction.is_none() || self.axis.is_some() {
                            return Err(MapError::InvalidBinding(
                                "stick_direction requires a direction and no axis".into(),
                            ));
                        }
                    }
                    _ => unreachable!("arm is StickAxis or StickDirection"),
                }
                Ok(())
            }
            _ => {
                if self.axis.is_some() || self.direction.is_some() {
                    return Err(MapError::InvalidBinding(
                        "axis and direction are only valid for stick shapes".into(),
                    ));
                }
                if self.controls.is_empty() {
                    return Err(MapError::InvalidBinding(
                        "controls must be non-empty".into(),
                    ));
                }
                match self.shape {
                    BindingShape::SinglePress if self.controls.len() != 1 => Err(
                        MapError::InvalidBinding("single_press requires one control".into()),
                    ),
                    BindingShape::Chord if self.controls.len() < 2 => Err(
                        MapError::InvalidBinding("chord requires at least two controls".into()),
                    ),
                    BindingShape::DoublePress
                        if !matches!(self.max_interval_ms, Some(100..=2000)) =>
                    {
                        Err(MapError::InvalidBinding(
                            "double_press requires max_interval_ms from 100 through 2000".into(),
                        ))
                    }
                    BindingShape::Hold if !matches!(self.min_duration_ms, Some(250..=5000)) => {
                        Err(MapError::InvalidBinding(
                            "hold requires min_duration_ms from 250 through 5000".into(),
                        ))
                    }
                    BindingShape::SinglePress | BindingShape::Chord
                        if self.max_interval_ms.is_some() || self.min_duration_ms.is_some() =>
                    {
                        Err(MapError::InvalidBinding(
                            "timing field is invalid for binding shape".into(),
                        ))
                    }
                    BindingShape::DoublePress if self.min_duration_ms.is_some() => {
                        Err(MapError::InvalidBinding(
                            "min_duration_ms is invalid for double_press".into(),
                        ))
                    }
                    BindingShape::Hold if self.max_interval_ms.is_some() => Err(
                        MapError::InvalidBinding("max_interval_ms is invalid for hold".into()),
                    ),
                    _ => Ok(()),
                }
            }
        }
    }

    fn signature(&self) -> BindingSignature<'_> {
        let mut controls: Vec<_> = self.controls.iter().map(String::as_str).collect();
        controls.sort_unstable();
        (
            self.shape.clone(),
            controls,
            self.max_interval_ms,
            self.min_duration_ms,
            self.axis,
            self.direction,
        )
    }

    fn id(&self) -> String {
        let controls = self.controls.join("+");
        match self.shape {
            BindingShape::SinglePress => format!("single_press({controls})"),
            BindingShape::Chord => format!("chord({controls})"),
            BindingShape::DoublePress => format!(
                "double_press({controls};{}ms)",
                self.max_interval_ms.unwrap_or_default()
            ),
            BindingShape::Hold => format!(
                "hold({controls};{}ms)",
                self.min_duration_ms.unwrap_or_default()
            ),
            BindingShape::StickAxis => format!(
                "stick_axis({controls};{})",
                self.axis.map_or("", Axis::as_str)
            ),
            BindingShape::StickDirection => format!(
                "stick_direction({controls};{})",
                self.direction.map_or("", Direction::as_str)
            ),
            BindingShape::OnScreen => "on_screen".into(),
        }
    }

    /// True when this binding is one of the ruled SystemMenu alternatives that the contract
    /// validator reserves from every application action.
    fn is_ruled_system_menu(&self) -> bool {
        let signature = self.signature();
        SYSTEM_MENU_SIGNATURES
            .iter()
            .any(|ruled| signature == ruled_binding(ruled).signature())
    }
}

/// (shape, controls, max_interval_ms, min_duration_ms) of a ruled SystemMenu binding.
type RuledMenuSpec = (
    &'static str,
    &'static [&'static str],
    Option<u32>,
    Option<u32>,
);

/// The seven ruled SystemMenu bindings, mirroring SYSTEM_MENU_BINDINGS in the platform
/// shell-input-contract validator.
const SYSTEM_MENU_SIGNATURES: [RuledMenuSpec; 7] = [
    ("single_press", &["guide"], None, None),
    ("single_press", &["home"], None, None),
    ("chord", &["select", "start"], None, None),
    ("hold", &["guide"], None, Some(1000)),
    ("double_press", &["select", "start"], Some(700), None),
    ("double_press", &["guide"], Some(500), None),
    ("hold", &["select", "l1", "r1"], None, Some(1000)),
];

fn ruled_binding(spec: &RuledMenuSpec) -> Binding {
    let (shape, controls, max_interval_ms, min_duration_ms) = *spec;
    Binding {
        shape: match shape {
            "single_press" => BindingShape::SinglePress,
            "chord" => BindingShape::Chord,
            "double_press" => BindingShape::DoublePress,
            "hold" => BindingShape::Hold,
            _ => unreachable!("ruled SystemMenu signatures use the four button shapes"),
        },
        controls: controls.iter().map(|c| (*c).to_string()).collect(),
        max_interval_ms,
        min_duration_ms,
        axis: None,
        direction: None,
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FallbackGlyph {
    pub source: String,
    pub id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PhysicalControl {
    pub position: String,
    #[serde(default)]
    pub printed_label: Option<String>,
    #[serde(default)]
    pub input_code: Option<String>,
    pub fallback_glyph: FallbackGlyph,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    pub context: String,
    pub action: String,
    pub binding: Binding,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DeviceContract {
    pub schema_version: u32,
    pub device_id: String,
    /// Contract v3 only: the device class whose shipped table the effective map realises.
    #[serde(default)]
    pub device_class: Option<String>,
    pub protected_actions: Vec<String>,
    pub physical_controls: Vec<PhysicalControl>,
    pub effective_map: Vec<Mapping>,
}

impl DeviceContract {
    pub fn parse_json(input: &str) -> Result<Self, MapError> {
        let contract: Self =
            serde_json::from_str(input).map_err(|e| MapError::InvalidContract(e.to_string()))?;
        contract.validate()?;
        Ok(contract)
    }

    pub fn validate(&self) -> Result<(), MapError> {
        if !CONTRACT_VERSIONS.contains(&self.schema_version) || self.device_id.is_empty() {
            return Err(MapError::InvalidContract(
                "unsupported schema or empty device identity".into(),
            ));
        }
        match self.schema_version {
            3 => {
                if !self
                    .device_class
                    .as_deref()
                    .is_some_and(|class| DEVICE_CLASSES.contains(&class))
                {
                    return Err(MapError::InvalidContract(
                        "contract v3 requires a known device_class".into(),
                    ));
                }
            }
            _ => {
                if self.device_class.is_some() {
                    return Err(MapError::InvalidContract(
                        "device_class requires contract schema version 3".into(),
                    ));
                }
            }
        }
        if self.protected_actions != ["SystemMenu"] {
            return Err(MapError::InvalidContract(
                "protected_actions must contain only SystemMenu".into(),
            ));
        }
        let controls: BTreeSet<_> = self
            .physical_controls
            .iter()
            .map(|c| c.position.as_str())
            .collect();
        if controls.len() != self.physical_controls.len() || self.physical_controls.is_empty() {
            return Err(MapError::InvalidContract(
                "physical control positions must be unique and non-empty".into(),
            ));
        }
        let v3 = self.schema_version == 3;
        for control in &self.physical_controls {
            let known: &[&str] = if v3 { &POSITIONS } else { &V2_POSITIONS };
            if !known.contains(&control.position.as_str()) {
                return Err(MapError::InvalidContract(format!(
                    "unknown physical position {} at schema version {}",
                    control.position, self.schema_version
                )));
            }
            if control.fallback_glyph.source != "pocketforge"
                || !control.fallback_glyph.id.starts_with("pf-")
            {
                return Err(MapError::InvalidContract(
                    "fallback glyph must be source-owned pf-*".into(),
                ));
            }
        }
        validate_map_policies(
            &self.effective_map,
            &controls,
            &system_menu_controls(&self.effective_map),
            v3,
        )?;
        validate_candidate(&self.effective_map, &controls)
    }

    fn controls(&self) -> BTreeSet<&str> {
        self.physical_controls
            .iter()
            .map(|c| c.position.as_str())
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize, Default, Eq, PartialEq, Serialize)]
struct PersistedDocument {
    schema_version: u32,
    #[serde(default)]
    devices: BTreeMap<String, Vec<Mapping>>,
}

pub trait RemapStore {
    fn load(&self, device_id: &str) -> Result<Option<Vec<Mapping>>, MapError>;
    fn save(&mut self, device_id: &str, mappings: &[Mapping]) -> Result<(), MapError>;
}

#[derive(Clone, Debug, Default)]
pub struct MemoryStore {
    devices: BTreeMap<String, Vec<Mapping>>,
    fail_save: bool,
}

impl MemoryStore {
    pub fn failing() -> Self {
        Self {
            fail_save: true,
            ..Self::default()
        }
    }
}
impl RemapStore for MemoryStore {
    fn load(&self, device_id: &str) -> Result<Option<Vec<Mapping>>, MapError> {
        Ok(self.devices.get(device_id).cloned())
    }
    fn save(&mut self, device_id: &str, mappings: &[Mapping]) -> Result<(), MapError> {
        if self.fail_save {
            return Err(MapError::Persistence("injected save failure".into()));
        }
        self.devices.insert(device_id.into(), mappings.to_vec());
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct JsonRemapStore {
    path: PathBuf,
}
impl JsonRemapStore {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}
impl RemapStore for JsonRemapStore {
    fn load(&self, device_id: &str) -> Result<Option<Vec<Mapping>>, MapError> {
        if !self.path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&self.path).map_err(io_error)?;
        let doc: PersistedDocument =
            serde_json::from_slice(&bytes).map_err(|e| MapError::Persistence(e.to_string()))?;
        let doc = migrate_persisted_document(doc)?;
        Ok(doc.devices.get(device_id).cloned())
    }
    fn save(&mut self, device_id: &str, mappings: &[Mapping]) -> Result<(), MapError> {
        let doc = if self.path.exists() {
            serde_json::from_slice::<PersistedDocument>(&fs::read(&self.path).map_err(io_error)?)
                .map_err(|e| MapError::Persistence(e.to_string()))?
        } else {
            PersistedDocument {
                schema_version: SCHEMA_VERSION,
                devices: BTreeMap::new(),
            }
        };
        let mut doc = migrate_persisted_document(doc)?;
        doc.devices.insert(device_id.into(), mappings.to_vec());
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(io_error)?;
        }
        let temp = temporary_path(&self.path);
        fs::write(
            &temp,
            serde_json::to_vec_pretty(&doc).map_err(|e| MapError::Persistence(e.to_string()))?,
        )
        .map_err(io_error)?;
        fs::rename(&temp, &self.path).map_err(io_error)
    }
}

fn migrate_persisted_document(
    mut document: PersistedDocument,
) -> Result<PersistedDocument, MapError> {
    match document.schema_version {
        1 => {
            for mappings in document.devices.values_mut() {
                for mapping in mappings {
                    if mapping.action == "SafeReturn" {
                        mapping.action = "SystemMenu".into();
                    }
                }
            }
            document.schema_version = SCHEMA_VERSION;
            Ok(document)
        }
        SCHEMA_VERSION => Ok(document),
        found => Err(MapError::UnsupportedVersion {
            found,
            supported: SCHEMA_VERSION,
        }),
    }
}

/// The router-owned system remap document (X16). The shell's per-session
/// `<state-dir>/remaps.json` is legacy; the router serves this path session-wide and
/// shell/Settings write through the router.
pub const SYSTEM_REMAP_PATH: &str = "/var/lib/pocketforge/input/remaps.json";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MigrationStatus {
    MigratedFromLegacy,
    AlreadyPresent,
    NoLegacyDocument,
}

/// Remap store for the system path with a one-time migration from the legacy shell path.
///
/// Reads prefer the system document; while it does not exist, a legacy shell document is
/// read and migrated in memory (never mutated in place). Saves always go to the system
/// path atomically. `migrate` performs the explicit, idempotent startup migration the
/// input router runs once; afterwards the legacy file is ignored and left untouched for
/// the router cutover (tsp-ihne1.7) to retire.
#[derive(Clone, Debug)]
pub struct SystemRemapStore {
    system: JsonRemapStore,
    legacy: Option<JsonRemapStore>,
}

impl SystemRemapStore {
    pub fn at(system: impl Into<PathBuf>) -> Self {
        Self {
            system: JsonRemapStore::at(system),
            legacy: None,
        }
    }
    pub fn system() -> Self {
        Self::at(SYSTEM_REMAP_PATH)
    }
    pub fn with_legacy(mut self, legacy: impl Into<PathBuf>) -> Self {
        self.legacy = Some(JsonRemapStore::at(legacy));
        self
    }
    pub fn system_path(&self) -> &Path {
        &self.system.path
    }
    pub fn migrate(&mut self) -> Result<MigrationStatus, MapError> {
        if self.system.path.exists() {
            return Ok(MigrationStatus::AlreadyPresent);
        }
        let Some(legacy) = &self.legacy else {
            return Ok(MigrationStatus::NoLegacyDocument);
        };
        if !legacy.path.exists() {
            return Ok(MigrationStatus::NoLegacyDocument);
        }
        let document = read_document(&legacy.path)?;
        write_document(&self.system.path, &document)?;
        Ok(MigrationStatus::MigratedFromLegacy)
    }
}

impl RemapStore for SystemRemapStore {
    fn load(&self, device_id: &str) -> Result<Option<Vec<Mapping>>, MapError> {
        if self.system.path.exists() {
            return self.system.load(device_id);
        }
        match &self.legacy {
            Some(legacy) => legacy.load(device_id),
            None => Ok(None),
        }
    }
    fn save(&mut self, device_id: &str, mappings: &[Mapping]) -> Result<(), MapError> {
        self.system.save(device_id, mappings)
    }
}

fn read_document(path: &Path) -> Result<PersistedDocument, MapError> {
    let bytes = fs::read(path).map_err(io_error)?;
    let document: PersistedDocument =
        serde_json::from_slice(&bytes).map_err(|e| MapError::Persistence(e.to_string()))?;
    migrate_persisted_document(document)
}

fn write_document(path: &Path, document: &PersistedDocument) -> Result<(), MapError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_error)?;
    }
    let temp = temporary_path(path);
    fs::write(
        &temp,
        serde_json::to_vec_pretty(document).map_err(|e| MapError::Persistence(e.to_string()))?,
    )
    .map_err(io_error)?;
    fs::rename(&temp, path).map_err(io_error)
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}
fn io_error(error: std::io::Error) -> MapError {
    MapError::Persistence(error.to_string())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MapEvent {
    BindingReResolved {
        action: String,
        stored_device_id: String,
        current_device_id: String,
        old_binding: Binding,
        effective_binding: Binding,
    },
    GlyphsUpdated {
        device_id: String,
        actions: Vec<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectiveMap {
    device_id: String,
    version: u32,
    controls: BTreeMap<String, PhysicalControl>,
    shipped: Vec<Mapping>,
    mappings: Vec<Mapping>,
    events: VecDeque<MapEvent>,
}

impl EffectiveMap {
    pub fn load<S: RemapStore>(contract: DeviceContract, store: &S) -> Result<Self, MapError> {
        let persisted = store.load(&contract.device_id)?;
        Self::from_persisted(contract, persisted.map(|m| (String::new(), m)))
    }

    /// Loads a map stored under a prior device identity and re-resolves it for `contract`.
    pub fn load_carried<S: RemapStore>(
        contract: DeviceContract,
        stored_device_id: &str,
        store: &S,
    ) -> Result<Self, MapError> {
        let persisted = store
            .load(stored_device_id)?
            .map(|mappings| (stored_device_id.to_owned(), mappings));
        Self::from_persisted(contract, persisted)
    }

    /// Loads a map carried from another device, applying the contract's portability rule.
    pub fn from_persisted(
        contract: DeviceContract,
        persisted: Option<(String, Vec<Mapping>)>,
    ) -> Result<Self, MapError> {
        contract.validate()?;
        let controls = contract.controls();
        let v3 = contract.schema_version == 3;
        let shipped = contract.effective_map.clone();
        let menu_controls = system_menu_controls(&shipped);
        let mut events = VecDeque::new();
        let stored_device = persisted
            .as_ref()
            .map(|(id, _)| id.as_str())
            .unwrap_or(&contract.device_id);

        let mut mappings = shipped.clone();
        if let Some((_, stored)) = &persisted {
            // A persisted candidate is untrusted startup input. A pair the shipped contract
            // does not know means the document predates or postdates this contract
            // generation; discard the whole document instead of guessing.
            let untrusted = stored.iter().any(|mapping| {
                !shipped
                    .iter()
                    .any(|s| s.context == mapping.context && s.action == mapping.action)
            });
            if untrusted {
                emit_re_resolved(
                    stored,
                    &shipped,
                    stored_device,
                    &contract.device_id,
                    &mut events,
                );
            } else {
                mappings = stored.clone();
                let shipped_menu = shipped
                    .iter()
                    .find(|mapping| mapping.action == "SystemMenu")
                    .expect("validated contract has exactly one SystemMenu")
                    .clone();
                if let Some(index) = mappings
                    .iter()
                    .position(|mapping| mapping.action == "SystemMenu")
                {
                    if mappings[index] != shipped_menu {
                        let old = std::mem::replace(&mut mappings[index], shipped_menu.clone());
                        events.push_back(MapEvent::BindingReResolved {
                            action: "SystemMenu".into(),
                            stored_device_id: stored_device.into(),
                            current_device_id: contract.device_id.clone(),
                            old_binding: old.binding,
                            effective_binding: shipped_menu.binding.clone(),
                        });
                    }
                    let mut kept_menu = false;
                    mappings.retain(|mapping| {
                        if mapping.action != "SystemMenu" {
                            true
                        } else if kept_menu {
                            false
                        } else {
                            kept_menu = true;
                            true
                        }
                    });
                }
                // A document saved by an older contract generation lacks the actions the
                // current one ships (e.g. the v3 keyboard and pointer rows). Backfill every
                // missing pair from the shipped map so no action is stranded at startup;
                // reset-to-default semantics for user-changed bindings are unchanged.
                for shipped_mapping in &shipped {
                    if !mappings.iter().any(|mapping| {
                        mapping.context == shipped_mapping.context
                            && mapping.action == shipped_mapping.action
                    }) {
                        mappings.push(shipped_mapping.clone());
                    }
                }
                // Every persisted row must satisfy the same policies a shipped contract
                // and an interactive remap satisfy. ANY violation — a malformed binding,
                // a duplicate pair, an on-screen/P2/Menu breach or a complete-map
                // invariant failure — discards the whole document and re-resolves to the
                // shipped map, so a corrupt remap can neither become effective nor strand
                // input at boot.
                let prepared = (|| -> Result<(), MapError> {
                    for mapping in &mut mappings {
                        mapping.binding.validate()?;
                        if mapping
                            .binding
                            .controls
                            .iter()
                            .any(|c| !controls.contains(c.as_str()))
                        {
                            let fallback = shipped
                                .iter()
                                .find(|m| {
                                    m.context == mapping.context && m.action == mapping.action
                                })
                                .ok_or_else(|| MapError::AbsentControl {
                                    action: mapping.action.clone(),
                                })?;
                            let old =
                                std::mem::replace(&mut mapping.binding, fallback.binding.clone());
                            events.push_back(MapEvent::BindingReResolved {
                                action: mapping.action.clone(),
                                stored_device_id: stored_device.into(),
                                current_device_id: contract.device_id.clone(),
                                old_binding: old,
                                effective_binding: fallback.binding.clone(),
                            });
                        }
                    }
                    validate_map_policies(&mappings, &controls, &menu_controls, v3)?;
                    validate_candidate(&mappings, &controls)
                })();
                if prepared.is_err() {
                    emit_re_resolved(
                        &mappings,
                        &shipped,
                        stored_device,
                        &contract.device_id,
                        &mut events,
                    );
                    mappings = shipped.clone();
                }
            }
        }
        Ok(Self {
            version: contract.schema_version,
            device_id: contract.device_id,
            controls: contract
                .physical_controls
                .into_iter()
                .map(|c| (c.position.clone(), c))
                .collect(),
            shipped,
            mappings,
            events,
        })
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }
    /// The contract schema version this map was loaded from; v3-only binding shapes are
    /// refused as remap candidates on a v2 map.
    pub fn contract_version(&self) -> u32 {
        self.version
    }
    pub fn mappings(&self) -> &[Mapping] {
        &self.mappings
    }
    /// Resolves a binding. `osk.num` is an inherited context: it never carries its own rows
    /// and resolves Type, Close, Backspace and Commit through `osk` (spec §4.1).
    pub fn binding(&self, context: &str, action: &str) -> Option<&Binding> {
        let context = if context == "osk.num" && OSK_NUM_INHERITED.contains(&action) {
            "osk"
        } else {
            context
        };
        self.mappings
            .iter()
            .find(|m| m.context == context && m.action == action)
            .map(|m| &m.binding)
    }
    pub fn next_event(&mut self) -> Option<MapEvent> {
        self.events.pop_front()
    }
    pub fn shipped_binding(&self, context: &str, action: &str) -> Option<&Binding> {
        self.shipped
            .iter()
            .find(|m| m.context == context && m.action == action)
            .map(|m| &m.binding)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemapPreview {
    pub context: String,
    pub action: String,
    pub old_binding: Binding,
    pub candidate_binding: Binding,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RollbackReason {
    Timeout,
    Interrupted,
    Reverted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransactionOutcome {
    Committed,
    RolledBack(RollbackReason),
}

pub struct RemapEngine<S> {
    map: EffectiveMap,
    store: S,
    pending: Option<RemapPreview>,
}

impl<S: RemapStore> RemapEngine<S> {
    pub fn new(map: EffectiveMap, store: S) -> Self {
        Self {
            map,
            store,
            pending: None,
        }
    }
    pub fn map(&self) -> &EffectiveMap {
        &self.map
    }
    pub fn map_mut(&mut self) -> &mut EffectiveMap {
        &mut self.map
    }
    pub fn begin(
        &mut self,
        context: &str,
        action: &str,
        candidate: Binding,
    ) -> Result<RemapPreview, MapError> {
        if self.pending.is_some() {
            return Err(MapError::TransactionActive);
        }
        if action == "SystemMenu" {
            return Err(MapError::ProtectedActionImmutable(action.into()));
        }
        candidate.validate()?;
        if self.map.version < 3
            && matches!(
                candidate.shape,
                BindingShape::StickAxis | BindingShape::StickDirection | BindingShape::OnScreen
            )
        {
            return Err(MapError::InvalidBinding(format!(
                "binding {} requires contract schema version 3",
                candidate.id()
            )));
        }
        if candidate.shape == BindingShape::OnScreen {
            if !KEYBOARD_CONTEXTS.contains(&context) {
                return Err(MapError::InvalidBinding(
                    "on_screen is only valid in the keyboard contexts".into(),
                ));
            }
            if REQUIRED_PHYSICAL_KEYBOARD.contains(&(context, action)) {
                return Err(MapError::RequiredPhysicalBinding(action.into()));
            }
        }
        // The Menu control and every ruled SystemMenu alternative stay reserved from all
        // other actions, in every context (spec §4.1/§4.2 rule P3).
        if candidate.is_ruled_system_menu()
            || system_menu_controls(&self.map.shipped)
                .iter()
                .any(|reserved| candidate.controls.iter().any(|c| c == reserved))
        {
            return Err(MapError::MenuReserved(candidate.id()));
        }
        if candidate
            .controls
            .iter()
            .any(|c| !self.map.controls.contains_key(c))
        {
            return Err(MapError::AbsentControl {
                action: action.into(),
            });
        }
        let index = mapping_index(&self.map.mappings, context, action)?;
        let preview = RemapPreview {
            context: context.into(),
            action: action.into(),
            old_binding: self.map.mappings[index].binding.clone(),
            candidate_binding: candidate,
        };
        validate_remap_collision(&self.map.mappings, &preview)?;
        let prospective = apply_preview(&self.map.mappings, &preview);
        validate_candidate(
            &prospective,
            &self.map.controls.keys().map(String::as_str).collect(),
        )?;
        self.pending = Some(preview.clone());
        Ok(preview)
    }
    pub fn confirm(&mut self) -> Result<TransactionOutcome, MapError> {
        let preview = self.pending.take().ok_or(MapError::NoTransaction)?;
        let prospective = apply_preview(&self.map.mappings, &preview);
        validate_candidate(
            &prospective,
            &self.map.controls.keys().map(String::as_str).collect(),
        )?;
        // Persistence is the commit point. The effective map and its glyph event remain untouched on failure.
        self.store.save(&self.map.device_id, &prospective)?;
        self.map.mappings = prospective;
        self.map.events.push_back(MapEvent::GlyphsUpdated {
            device_id: self.map.device_id.clone(),
            actions: vec![preview.action],
        });
        Ok(TransactionOutcome::Committed)
    }
    /// Restores every binding shipped by the device contract in one atomic commit.
    pub fn reset_to_shipped(&mut self) -> Result<(), MapError> {
        self.pending = None;

        let shipped = self.map.shipped.clone();
        let mut changed_actions: Vec<_> = shipped
            .iter()
            .filter(|mapping| {
                self.map
                    .binding(&mapping.context, &mapping.action)
                    .filter(|binding| *binding == &mapping.binding)
                    .is_none()
            })
            .map(|mapping| mapping.action.clone())
            .collect();
        changed_actions.extend(
            self.map
                .mappings
                .iter()
                .filter(|mapping| {
                    self.map
                        .shipped_binding(&mapping.context, &mapping.action)
                        .is_none()
                })
                .map(|mapping| mapping.action.clone()),
        );

        // Persistence is the commit point: do not expose the shipped map unless the single
        // complete-map save succeeds.
        self.store.save(&self.map.device_id, &shipped)?;
        self.map.mappings = shipped;
        self.map.events.push_back(MapEvent::GlyphsUpdated {
            device_id: self.map.device_id.clone(),
            actions: changed_actions,
        });
        Ok(())
    }
    pub fn timeout(&mut self) -> Result<TransactionOutcome, MapError> {
        self.rollback(RollbackReason::Timeout)
    }
    pub fn interrupt(&mut self) -> Result<TransactionOutcome, MapError> {
        self.rollback(RollbackReason::Interrupted)
    }
    pub fn revert(&mut self) -> Result<TransactionOutcome, MapError> {
        self.rollback(RollbackReason::Reverted)
    }
    fn rollback(&mut self, reason: RollbackReason) -> Result<TransactionOutcome, MapError> {
        self.pending.take().ok_or(MapError::NoTransaction)?;
        Ok(TransactionOutcome::RolledBack(reason))
    }
}

fn apply_preview(mappings: &[Mapping], preview: &RemapPreview) -> Vec<Mapping> {
    let mut candidate = mappings.to_vec();
    if let Ok(index) = mapping_index(&candidate, &preview.context, &preview.action) {
        candidate[index].binding = preview.candidate_binding.clone();
    }
    candidate
}
fn mapping_index(mappings: &[Mapping], context: &str, action: &str) -> Result<usize, MapError> {
    mappings
        .iter()
        .position(|m| m.context == context && m.action == action)
        .ok_or_else(|| MapError::UnknownAction {
            context: context.into(),
            action: action.into(),
        })
}

fn validate_remap_collision(mappings: &[Mapping], preview: &RemapPreview) -> Result<(), MapError> {
    for existing in mappings.iter().filter(|mapping| {
        !(mapping.context == preview.context && mapping.action == preview.action)
            && (mapping.context == preview.context
                || mapping.context == "global"
                || preview.context == "global")
    }) {
        let either_is_protected = PROTECTED.contains(&preview.action.as_str())
            || PROTECTED.contains(&existing.action.as_str());
        let both_are_face_actions = FACE_ACTIONS.contains(&preview.action.as_str())
            && FACE_ACTIONS.contains(&existing.action.as_str());
        if either_is_protected
            && both_are_face_actions
            && preview.candidate_binding.signature() == existing.binding.signature()
        {
            return Err(MapError::Collision {
                first: preview.action.clone(),
                second: existing.action.clone(),
            });
        }
    }
    Ok(())
}

/// The per-mapping policy every effective map must satisfy, shared by contract validation
/// and the persisted-document path so a stored remap can never smuggle in what an
/// interactive remap or a shipped contract would refuse: uniqueness, the version-gated
/// vocabularies, binding shape rules, absent controls, the Menu reservation and the
/// on-screen placement/P2 rules.
fn validate_map_policies(
    mappings: &[Mapping],
    controls: &BTreeSet<&str>,
    menu_controls: &[&str],
    v3: bool,
) -> Result<(), MapError> {
    let mut seen_pairs = BTreeSet::new();
    for mapping in mappings {
        if !seen_pairs.insert((mapping.context.as_str(), mapping.action.as_str())) {
            return Err(MapError::InvalidContract(format!(
                "duplicate mapping for {}/{}",
                mapping.context, mapping.action
            )));
        }
        if !v3
            && (!V2_CONTEXTS.contains(&mapping.context.as_str())
                || !V2_ACTIONS.contains(&mapping.action.as_str()))
        {
            return Err(MapError::InvalidContract(format!(
                "context {} or action {} requires contract schema version 3",
                mapping.context, mapping.action
            )));
        }
        if !ACTIONS.contains(&mapping.action.as_str())
            || !allowed_contexts(&mapping.action)
                .is_some_and(|contexts| contexts.contains(&mapping.context.as_str()))
        {
            return Err(MapError::InvalidContract(format!(
                "unknown semantic action {} in context {}",
                mapping.action, mapping.context
            )));
        }
        mapping.binding.validate()?;
        if !v3
            && matches!(
                mapping.binding.shape,
                BindingShape::StickAxis | BindingShape::StickDirection | BindingShape::OnScreen
            )
        {
            return Err(MapError::InvalidBinding(format!(
                "binding {} requires contract schema version 3",
                mapping.binding.id()
            )));
        }
        if mapping
            .binding
            .controls
            .iter()
            .any(|c| !controls.contains(c.as_str()))
        {
            return Err(MapError::AbsentControl {
                action: mapping.action.clone(),
            });
        }
        let is_menu = mapping.action == "SystemMenu";
        if is_menu && mapping.context != "global" {
            return Err(MapError::InvalidContract(
                "SystemMenu must use the global context".into(),
            ));
        }
        if !is_menu
            && (mapping.binding.is_ruled_system_menu()
                || menu_controls
                    .iter()
                    .any(|reserved| mapping.binding.controls.iter().any(|c| c == reserved)))
        {
            return Err(MapError::MenuReserved(mapping.binding.id()));
        }
        if mapping.binding.shape == BindingShape::OnScreen {
            if !KEYBOARD_CONTEXTS.contains(&mapping.context.as_str()) {
                return Err(MapError::InvalidBinding(
                    "on_screen is only valid in the keyboard contexts".into(),
                ));
            }
            if REQUIRED_PHYSICAL_KEYBOARD
                .contains(&(mapping.context.as_str(), mapping.action.as_str()))
            {
                return Err(MapError::RequiredPhysicalBinding(mapping.action.clone()));
            }
        }
    }
    Ok(())
}

fn validate_candidate(mappings: &[Mapping], controls: &BTreeSet<&str>) -> Result<(), MapError> {
    for (context, action) in PROTECTED_PAIRS {
        let matching: Vec<_> = mappings
            .iter()
            .filter(|m| m.context == context && m.action == action)
            .collect();
        if matching.len() != 1
            || matching[0]
                .binding
                .controls
                .iter()
                .any(|c| !controls.contains(c.as_str()))
        {
            return Err(MapError::ProtectedActionUnreachable(action.into()));
        }
    }
    for (index, protected) in mappings
        .iter()
        .enumerate()
        .filter(|(_, m)| PROTECTED.contains(&m.action.as_str()))
    {
        for (other_index, face) in mappings.iter().enumerate().filter(|(_, m)| {
            FACE_ACTIONS.contains(&m.action.as_str())
                && (protected.context == m.context
                    || protected.context == "global"
                    || m.context == "global")
        }) {
            if index != other_index && protected.binding.signature() == face.binding.signature() {
                return Err(MapError::Collision {
                    first: protected.action.clone(),
                    second: face.action.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Emits one honest re-resolution event for every persisted binding that differs from the
/// shipped default of the same (context, action) pair.
fn emit_re_resolved(
    persisted: &[Mapping],
    shipped: &[Mapping],
    stored_device_id: &str,
    current_device_id: &str,
    events: &mut VecDeque<MapEvent>,
) {
    for mapping in persisted {
        if let Some(fallback) = shipped
            .iter()
            .find(|m| m.context == mapping.context && m.action == mapping.action)
        {
            if mapping.binding != fallback.binding {
                events.push_back(MapEvent::BindingReResolved {
                    action: mapping.action.clone(),
                    stored_device_id: stored_device_id.into(),
                    current_device_id: current_device_id.into(),
                    old_binding: mapping.binding.clone(),
                    effective_binding: fallback.binding.clone(),
                });
            }
        }
    }
}

/// The controls a shipped single-press SystemMenu binding dedicates to the system menu.
/// Chord menus (e.g. select+start) do not reserve their individual controls: the same
/// fixture legitimately ships global/Start on `start` next to the chord.
fn system_menu_controls(mappings: &[Mapping]) -> Vec<&str> {
    mappings
        .iter()
        .find(|m| m.action == "SystemMenu")
        .filter(|m| m.binding.shape == BindingShape::SinglePress)
        .map(|m| m.binding.controls.iter().map(String::as_str).collect())
        .unwrap_or_default()
}

impl GlyphResolver for EffectiveMap {
    fn resolve(&self, action: &ShellAction) -> Result<GlyphResult, GlyphError> {
        let action = semantic_name(action);
        // Action names can repeat across contexts (`Back` lives in shell, osk.edit and
        // pointer). Shell-facing consumers ask without a context, so the shell's own
        // contexts win in a fixed priority order.
        let mapping = GLYPH_CONTEXT_PRIORITY.iter().find_map(|context| {
            self.mappings
                .iter()
                .find(|m| m.context == *context && m.action == action)
        });
        let Some(mapping) = mapping else {
            return Ok(GlyphResult::UnsupportedAction);
        };
        let mut labels = Vec::new();
        let mut fallbacks = Vec::new();
        for id in &mapping.binding.controls {
            let control = self.controls.get(id).ok_or(GlyphError::InvalidBinding)?;
            if let Some(label) = &control.printed_label {
                labels.push(label.clone());
            }
            fallbacks.push(control.fallback_glyph.id.clone());
        }
        let separator = " + ";
        let printed_label = if labels.len() == mapping.binding.controls.len() {
            labels.join(separator)
        } else {
            String::new()
        };
        Ok(GlyphResult::Resolved(EffectiveBinding {
            binding_id: mapping.binding.id(),
            printed_label,
            source_fallback: fallbacks.join(separator),
        }))
    }
}

fn semantic_name(action: &ShellAction) -> &str {
    match action {
        ShellAction::Move(AxisMove::Up) => "Move.up",
        ShellAction::Move(AxisMove::Down) => "Move.down",
        ShellAction::Move(AxisMove::Left) => "Move.left",
        ShellAction::Move(AxisMove::Right) => "Move.right",
        ShellAction::Activate => "Activate",
        ShellAction::Back => "Back",
        ShellAction::Custom(name) => name,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MapError {
    InvalidContract(String),
    InvalidBinding(String),
    AbsentControl {
        action: String,
    },
    ProtectedActionImmutable(String),
    ProtectedActionUnreachable(String),
    /// The candidate uses the Menu control or a ruled SystemMenu binding (spec §4.2 P3).
    MenuReserved(String),
    /// Rule P2: the action must keep a physical binding; on-screen-only was refused.
    RequiredPhysicalBinding(String),
    Collision {
        first: String,
        second: String,
    },
    UnknownAction {
        context: String,
        action: String,
    },
    TransactionActive,
    NoTransaction,
    UnsupportedVersion {
        found: u32,
        supported: u32,
    },
    Persistence(String),
}
impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for MapError {}

#[cfg(test)]
mod tests;
