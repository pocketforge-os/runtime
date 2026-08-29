//! Effective input maps, transactional remapping, and glyph resolution.
//!
//! The crate consumes the frozen `shell-input-contract` JSON shape. It owns policy and
//! persistence, but deliberately does not read input devices or render glyphs.

use pf_ports::{EffectiveBinding, GlyphError, GlyphResolver, GlyphResult, ShellAction};
use pf_scene::AxisMove;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: u32 = 1;
const PROTECTED: [&str; 3] = ["Activate", "Back", "SafeReturn"];
const FACE_ACTIONS: [&str; 6] = [
    "Activate",
    "Back",
    "Quick",
    "Search.open",
    "Search.submit",
    "Search.cancel",
];
const ACTIONS: [&str; 11] = [
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
    "SafeReturn",
];
const POSITIONS: [&str; 12] = [
    "east", "south", "west", "north", "start", "select", "guide", "home", "l1", "r1", "l2", "r2",
];

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingShape {
    SinglePress,
    Chord,
    DoublePress,
    Hold,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub shape: BindingShape,
    pub controls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_interval_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_duration_ms: Option<u32>,
}

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
        }
    }

    fn validate(&self) -> Result<(), MapError> {
        if self.controls.is_empty() || self.controls.iter().any(String::is_empty) {
            return Err(MapError::InvalidBinding(
                "controls must be non-empty".into(),
            ));
        }
        let unique: BTreeSet<_> = self.controls.iter().collect();
        if unique.len() != self.controls.len() {
            return Err(MapError::InvalidBinding("controls must be unique".into()));
        }
        match self.shape {
            BindingShape::SinglePress if self.controls.len() != 1 => Err(MapError::InvalidBinding(
                "single_press requires one control".into(),
            )),
            BindingShape::Chord if self.controls.len() < 2 => Err(MapError::InvalidBinding(
                "chord requires at least two controls".into(),
            )),
            BindingShape::DoublePress if !matches!(self.max_interval_ms, Some(100..=2000)) => {
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
            BindingShape::DoublePress if self.min_duration_ms.is_some() => Err(
                MapError::InvalidBinding("min_duration_ms is invalid for double_press".into()),
            ),
            BindingShape::Hold if self.max_interval_ms.is_some() => Err(MapError::InvalidBinding(
                "max_interval_ms is invalid for hold".into(),
            )),
            _ => Ok(()),
        }
    }

    fn signature(&self) -> (BindingShape, Vec<&str>, Option<u32>, Option<u32>) {
        let mut controls: Vec<_> = self.controls.iter().map(String::as_str).collect();
        controls.sort_unstable();
        (
            self.shape.clone(),
            controls,
            self.max_interval_ms,
            self.min_duration_ms,
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
        }
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
        if self.schema_version != SCHEMA_VERSION || self.device_id.is_empty() {
            return Err(MapError::InvalidContract(
                "unsupported schema or empty device identity".into(),
            ));
        }
        if self.protected_actions != ["SafeReturn"] {
            return Err(MapError::InvalidContract(
                "protected_actions must contain only SafeReturn".into(),
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
        for control in &self.physical_controls {
            if !POSITIONS.contains(&control.position.as_str()) {
                return Err(MapError::InvalidContract(format!(
                    "unknown physical position {}",
                    control.position
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
        for mapping in &self.effective_map {
            if mapping.context.is_empty() || !ACTIONS.contains(&mapping.action.as_str()) {
                return Err(MapError::InvalidContract(format!(
                    "unknown semantic action {}",
                    mapping.action
                )));
            }
            mapping.binding.validate()?;
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
        }
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
        if doc.schema_version != SCHEMA_VERSION {
            return Err(MapError::Persistence("unsupported schema version".into()));
        }
        Ok(doc.devices.get(device_id).cloned())
    }
    fn save(&mut self, device_id: &str, mappings: &[Mapping]) -> Result<(), MapError> {
        let mut doc = if self.path.exists() {
            serde_json::from_slice::<PersistedDocument>(&fs::read(&self.path).map_err(io_error)?)
                .map_err(|e| MapError::Persistence(e.to_string()))?
        } else {
            PersistedDocument {
                schema_version: SCHEMA_VERSION,
                devices: BTreeMap::new(),
            }
        };
        doc.schema_version = SCHEMA_VERSION;
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
        let mut mappings = persisted
            .as_ref()
            .map(|(_, m)| m.clone())
            .unwrap_or_else(|| contract.effective_map.clone());
        let mut events = VecDeque::new();
        let stored_device = persisted
            .as_ref()
            .map(|(id, _)| id.as_str())
            .unwrap_or(&contract.device_id);
        for mapping in &mut mappings {
            mapping.binding.validate()?;
            if mapping
                .binding
                .controls
                .iter()
                .any(|c| !controls.contains(c.as_str()))
            {
                let fallback = contract
                    .effective_map
                    .iter()
                    .find(|m| m.context == mapping.context && m.action == mapping.action)
                    .ok_or_else(|| MapError::AbsentControl {
                        action: mapping.action.clone(),
                    })?;
                let old = std::mem::replace(&mut mapping.binding, fallback.binding.clone());
                events.push_back(MapEvent::BindingReResolved {
                    action: mapping.action.clone(),
                    stored_device_id: stored_device.into(),
                    current_device_id: contract.device_id.clone(),
                    old_binding: old,
                    effective_binding: fallback.binding.clone(),
                });
            }
        }
        validate_candidate(&mappings, &controls)?;
        Ok(Self {
            device_id: contract.device_id,
            controls: contract
                .physical_controls
                .into_iter()
                .map(|c| (c.position.clone(), c))
                .collect(),
            shipped: contract.effective_map,
            mappings,
            events,
        })
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }
    pub fn mappings(&self) -> &[Mapping] {
        &self.mappings
    }
    pub fn binding(&self, context: &str, action: &str) -> Option<&Binding> {
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
        candidate.validate()?;
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

fn validate_candidate(mappings: &[Mapping], controls: &BTreeSet<&str>) -> Result<(), MapError> {
    for action in PROTECTED {
        let matching: Vec<_> = mappings.iter().filter(|m| m.action == action).collect();
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
    for safe in mappings.iter().filter(|m| m.action == "SafeReturn") {
        for face in mappings.iter().filter(|m| {
            FACE_ACTIONS.contains(&m.action.as_str())
                && (safe.context == "global" || safe.context == m.context)
        }) {
            if safe.binding.signature() == face.binding.signature() {
                return Err(MapError::Collision {
                    first: safe.action.clone(),
                    second: face.action.clone(),
                });
            }
        }
    }
    Ok(())
}

impl GlyphResolver for EffectiveMap {
    fn resolve(&self, action: &ShellAction) -> Result<GlyphResult, GlyphError> {
        let action = semantic_name(action);
        let Some(mapping) = self.mappings.iter().find(|m| m.action == action) else {
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
    AbsentControl { action: String },
    ProtectedActionUnreachable(String),
    Collision { first: String, second: String },
    UnknownAction { context: String, action: String },
    TransactionActive,
    NoTransaction,
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
