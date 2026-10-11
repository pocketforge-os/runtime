//! The menu model: Menu › Page › Section › Item, as data.
//!
//! The JSON form is `menu-page.schema.json` in the platform repo. Items are a flat object with
//! a `kind` tag; fields that do not belong to the kind are refused, so a fixture cannot carry a
//! silently ignored key.

use crate::kind::ItemKindId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// The `format` value of a v1 menu document.
pub const FORMAT_V1: &str = "pocketforge.menu/1";

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct PageId(pub String);

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ItemId(pub String);

impl fmt::Display for PageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A user-visible string: a message id with arguments (system strings, Fluent later), or a
/// literal (app runtime items arrive already translated).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Msg {
    Id(MsgId),
    Text(MsgText),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MsgId {
    pub id: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub args: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MsgText {
    pub text: String,
}

impl Msg {
    pub fn id(id: &str) -> Self {
        Msg::Id(MsgId {
            id: id.to_string(),
            args: BTreeMap::new(),
        })
    }

    pub fn text(text: &str) -> Self {
        Msg::Text(MsgText {
            text: text.to_string(),
        })
    }
}

/// A visibility or enabled rule over named host predicates (`has_bt`, `app_running`, …;
/// every-menu-spec §0). Unknown predicates evaluate false.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Cond {
    #[default]
    Always,
    Never,
    When(String),
    Unless(String),
    All(Vec<Cond>),
    Any(Vec<Cond>),
}

impl Cond {
    pub fn is_always(&self) -> bool {
        matches!(self, Cond::Always)
    }

    /// Evaluate with `flag(name)`. Does not allocate.
    pub fn eval(&self, flag: &dyn Fn(&str) -> bool) -> bool {
        match self {
            Cond::Always => true,
            Cond::Never => false,
            Cond::When(name) => flag(name),
            Cond::Unless(name) => !flag(name),
            Cond::All(all) => all.iter().all(|c| c.eval(flag)),
            Cond::Any(any) => any.iter().any(|c| c.eval(flag)),
        }
    }

    pub(crate) fn predicates<'a>(&'a self, out: &mut BTreeSet<&'a str>) {
        match self {
            Cond::Always | Cond::Never => {}
            Cond::When(name) | Cond::Unless(name) => {
                out.insert(name);
            }
            Cond::All(list) | Cond::Any(list) => list.iter().for_each(|c| c.predicates(out)),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum RawCond {
    Word(String),
    Rule(RawRule),
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum RawRule {
    When(String),
    Unless(String),
    All(Vec<Cond>),
    Any(Vec<Cond>),
}

impl Serialize for Cond {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let raw = match self {
            Cond::Always => RawCond::Word("always".into()),
            Cond::Never => RawCond::Word("never".into()),
            Cond::When(p) => RawCond::Rule(RawRule::When(p.clone())),
            Cond::Unless(p) => RawCond::Rule(RawRule::Unless(p.clone())),
            Cond::All(l) => RawCond::Rule(RawRule::All(l.clone())),
            Cond::Any(l) => RawCond::Rule(RawRule::Any(l.clone())),
        };
        raw.serialize(s)
    }
}

impl<'de> Deserialize<'de> for Cond {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match RawCond::deserialize(d)? {
            RawCond::Word(w) if w == "always" => Ok(Cond::Always),
            RawCond::Word(w) if w == "never" => Ok(Cond::Never),
            RawCond::Word(w) => Err(serde::de::Error::custom(format!(
                "condition must be \"always\", \"never\" or a rule object, got {w:?}"
            ))),
            RawCond::Rule(RawRule::When(p)) => Ok(Cond::When(p)),
            RawCond::Rule(RawRule::Unless(p)) => Ok(Cond::Unless(p)),
            RawCond::Rule(RawRule::All(l)) => Ok(Cond::All(l)),
            RawCond::Rule(RawRule::Any(l)) => Ok(Cond::Any(l)),
        }
    }
}

/// Where an item's value lives.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Bind {
    /// A pf-prefs key (validated against its `PrefSpec`).
    Pref(String),
    /// A declared app option key (M9).
    #[serde(rename = "option")]
    AppOption(String),
    /// A host-owned value (volume, Wi-Fi power, the app's runtime items).
    Host(String),
}

/// Confirm card for an action. Focus starts on Cancel.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Confirm {
    pub title: Msg,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Msg>,
    pub confirm_label: Msg,
    #[serde(default, skip_serializing_if = "is_false")]
    pub destructive: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChoiceOption {
    pub value: String,
    pub label: Msg,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Msg>,
}

/// How a choice row presents its value. Left/right cycle in both; A opens the picker.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Present {
    #[default]
    Inline,
    Picker,
}

/// The system keyboard's purpose hint for a text item (maps to the SDK's OSK purpose).
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TextPurpose {
    #[default]
    Normal,
    Name,
    Number,
    Pin,
    Email,
    Url,
    Search,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DateTimeFields {
    Date,
    Time,
    #[default]
    DateTime,
}

/// The kind-specific part of an item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ItemKind {
    Action {
        confirm: Option<Confirm>,
        stay_open: bool,
    },
    Toggle {
        bind: Bind,
    },
    Choice {
        bind: Bind,
        options: Vec<ChoiceOption>,
        present: Present,
    },
    Stepper {
        bind: Bind,
        min: i64,
        max: i64,
        step: i64,
        unit: Option<Msg>,
    },
    Info {
        value: Option<Msg>,
        bind: Option<Bind>,
    },
    Slider {
        bind: Bind,
        min: i64,
        max: i64,
        step: i64,
    },
    Text {
        bind: Bind,
        purpose: TextPurpose,
        max_chars: u32,
        secret: bool,
    },
    Link {
        page: PageId,
    },
    Progress {
        bind: Bind,
        indeterminate: bool,
    },
    Binding {
        context: String,
        action: String,
    },
    DateTime {
        bind: Bind,
        fields: DateTimeFields,
    },
    Custom {
        renderer: String,
    },
}

impl ItemKind {
    pub fn id(&self) -> ItemKindId {
        match self {
            ItemKind::Action { .. } => ItemKindId::Action,
            ItemKind::Toggle { .. } => ItemKindId::Toggle,
            ItemKind::Choice { .. } => ItemKindId::Choice,
            ItemKind::Stepper { .. } => ItemKindId::Stepper,
            ItemKind::Info { .. } => ItemKindId::Info,
            ItemKind::Slider { .. } => ItemKindId::Slider,
            ItemKind::Text { .. } => ItemKindId::Text,
            ItemKind::Link { .. } => ItemKindId::Link,
            ItemKind::Progress { .. } => ItemKindId::Progress,
            ItemKind::Binding { .. } => ItemKindId::Binding,
            ItemKind::DateTime { .. } => ItemKindId::DateTime,
            ItemKind::Custom { .. } => ItemKindId::Custom,
        }
    }

    /// The value binding, if the kind has one.
    pub fn bind(&self) -> Option<&Bind> {
        match self {
            ItemKind::Toggle { bind }
            | ItemKind::Choice { bind, .. }
            | ItemKind::Stepper { bind, .. }
            | ItemKind::Slider { bind, .. }
            | ItemKind::Text { bind, .. }
            | ItemKind::Progress { bind, .. }
            | ItemKind::DateTime { bind, .. } => Some(bind),
            ItemKind::Info { bind, .. } => bind.as_ref(),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Item {
    pub id: ItemId,
    pub label: Msg,
    pub subtitle: Option<Msg>,
    pub icon: Option<String>,
    pub enabled: Cond,
    pub visible: Cond,
    pub a11y: Option<Msg>,
    pub kind: ItemKind,
}

impl Item {
    pub fn new(id: &str, label: Msg, kind: ItemKind) -> Self {
        Item {
            id: ItemId(id.to_string()),
            label,
            subtitle: None,
            icon: None,
            enabled: Cond::Always,
            visible: Cond::Always,
            a11y: None,
            kind,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<Msg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footer: Option<Msg>,
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub id: PageId,
    pub title: Msg,
    /// The tab group this page belongs to, in order, including itself. Every page in the
    /// group lists the same group.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<PageId>,
    /// The action Start runs on this page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<ItemId>,
    pub sections: Vec<Section>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Menu {
    pub format: String,
    pub id: String,
    pub root: PageId,
    pub pages: Vec<Page>,
}

impl Menu {
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    pub fn to_json_pretty(&self) -> String {
        let mut out = serde_json::to_string_pretty(self).expect("menu model always serializes");
        out.push('\n');
        out
    }

    pub fn page(&self, id: &PageId) -> Option<&Page> {
        self.pages.iter().find(|page| &page.id == id)
    }

    pub fn items(&self) -> impl Iterator<Item = &Item> {
        self.pages
            .iter()
            .flat_map(|p| p.sections.iter())
            .flat_map(|s| s.items.iter())
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// The flat JSON form of an item. Every field is optional here; [`Item`]'s conversion refuses
/// fields that do not belong to the kind and requires the ones that do.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawItem {
    id: ItemId,
    kind: ItemKindId,
    label: Msg,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subtitle: Option<Msg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    enabled: Option<Cond>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    visible: Option<Cond>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    a11y: Option<Msg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bind: Option<Bind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    confirm: Option<Confirm>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stay_open: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    options: Option<Vec<ChoiceOption>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    present: Option<Present>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    min: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    step: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unit: Option<Msg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    value: Option<Msg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    purpose: Option<TextPurpose>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_chars: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    secret: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    page: Option<PageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    indeterminate: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fields: Option<DateTimeFields>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    renderer: Option<String>,
}

impl RawItem {
    /// Names of the kind-specific fields that are set.
    fn present_fields(&self) -> Vec<&'static str> {
        let mut set = Vec::new();
        let mut note = |on: bool, name: &'static str| {
            if on {
                set.push(name);
            }
        };
        note(self.bind.is_some(), "bind");
        note(self.confirm.is_some(), "confirm");
        note(self.stay_open.is_some(), "stay_open");
        note(self.options.is_some(), "options");
        note(self.present.is_some(), "present");
        note(self.min.is_some(), "min");
        note(self.max.is_some(), "max");
        note(self.step.is_some(), "step");
        note(self.unit.is_some(), "unit");
        note(self.value.is_some(), "value");
        note(self.purpose.is_some(), "purpose");
        note(self.max_chars.is_some(), "max_chars");
        note(self.secret.is_some(), "secret");
        note(self.page.is_some(), "page");
        note(self.indeterminate.is_some(), "indeterminate");
        note(self.context.is_some(), "context");
        note(self.action.is_some(), "action");
        note(self.fields.is_some(), "fields");
        note(self.renderer.is_some(), "renderer");
        set
    }
}

/// The fields each kind accepts (beyond the common ones).
pub(crate) fn kind_fields(kind: ItemKindId) -> &'static [&'static str] {
    match kind {
        ItemKindId::Action => &["confirm", "stay_open"],
        ItemKindId::Toggle => &["bind"],
        ItemKindId::Choice => &["bind", "options", "present"],
        ItemKindId::Stepper => &["bind", "min", "max", "step", "unit"],
        ItemKindId::Info => &["value", "bind"],
        ItemKindId::Slider => &["bind", "min", "max", "step"],
        ItemKindId::Text => &["bind", "purpose", "max_chars", "secret"],
        ItemKindId::Link => &["page"],
        ItemKindId::Progress => &["bind", "indeterminate"],
        ItemKindId::Binding => &["context", "action"],
        ItemKindId::DateTime => &["bind", "fields"],
        ItemKindId::Custom => &["renderer"],
    }
}

fn need<T>(value: Option<T>, kind: ItemKindId, field: &str) -> Result<T, String> {
    value.ok_or_else(|| format!("{kind} item needs `{field}`"))
}

impl TryFrom<RawItem> for Item {
    type Error = String;

    fn try_from(raw: RawItem) -> Result<Self, String> {
        let kind_id = raw.kind;
        let allowed = kind_fields(kind_id);
        if let Some(extra) = raw
            .present_fields()
            .into_iter()
            .find(|f| !allowed.contains(f))
        {
            return Err(format!(
                "item {:?}: `{extra}` is not a field of {kind_id} items",
                raw.id.0
            ));
        }
        let k = kind_id;
        let kind = match kind_id {
            ItemKindId::Action => ItemKind::Action {
                confirm: raw.confirm,
                stay_open: raw.stay_open.unwrap_or(false),
            },
            ItemKindId::Toggle => ItemKind::Toggle {
                bind: need(raw.bind, k, "bind")?,
            },
            ItemKindId::Choice => ItemKind::Choice {
                bind: need(raw.bind, k, "bind")?,
                options: need(raw.options, k, "options")?,
                present: raw.present.unwrap_or_default(),
            },
            ItemKindId::Stepper => ItemKind::Stepper {
                bind: need(raw.bind, k, "bind")?,
                min: need(raw.min, k, "min")?,
                max: need(raw.max, k, "max")?,
                step: raw.step.unwrap_or(1),
                unit: raw.unit,
            },
            ItemKindId::Info => ItemKind::Info {
                value: raw.value,
                bind: raw.bind,
            },
            ItemKindId::Slider => ItemKind::Slider {
                bind: need(raw.bind, k, "bind")?,
                min: need(raw.min, k, "min")?,
                max: need(raw.max, k, "max")?,
                step: raw.step.unwrap_or(1),
            },
            ItemKindId::Text => ItemKind::Text {
                bind: need(raw.bind, k, "bind")?,
                purpose: raw.purpose.unwrap_or_default(),
                max_chars: need(raw.max_chars, k, "max_chars")?,
                secret: raw.secret.unwrap_or(false),
            },
            ItemKindId::Link => ItemKind::Link {
                page: need(raw.page, k, "page")?,
            },
            ItemKindId::Progress => ItemKind::Progress {
                bind: need(raw.bind, k, "bind")?,
                indeterminate: raw.indeterminate.unwrap_or(false),
            },
            ItemKindId::Binding => ItemKind::Binding {
                context: need(raw.context, k, "context")?,
                action: need(raw.action, k, "action")?,
            },
            ItemKindId::DateTime => ItemKind::DateTime {
                bind: need(raw.bind, k, "bind")?,
                fields: raw.fields.unwrap_or_default(),
            },
            ItemKindId::Custom => ItemKind::Custom {
                renderer: need(raw.renderer, k, "renderer")?,
            },
        };
        Ok(Item {
            id: raw.id,
            label: raw.label,
            subtitle: raw.subtitle,
            icon: raw.icon,
            enabled: raw.enabled.unwrap_or_default(),
            visible: raw.visible.unwrap_or_default(),
            a11y: raw.a11y,
            kind,
        })
    }
}

impl From<&Item> for RawItem {
    fn from(item: &Item) -> Self {
        let mut raw = RawItem {
            id: item.id.clone(),
            kind: item.kind.id(),
            label: item.label.clone(),
            subtitle: item.subtitle.clone(),
            icon: item.icon.clone(),
            enabled: (!item.enabled.is_always()).then(|| item.enabled.clone()),
            visible: (!item.visible.is_always()).then(|| item.visible.clone()),
            a11y: item.a11y.clone(),
            bind: None,
            confirm: None,
            stay_open: None,
            options: None,
            present: None,
            min: None,
            max: None,
            step: None,
            unit: None,
            value: None,
            purpose: None,
            max_chars: None,
            secret: None,
            page: None,
            indeterminate: None,
            context: None,
            action: None,
            fields: None,
            renderer: None,
        };
        match &item.kind {
            ItemKind::Action { confirm, stay_open } => {
                raw.confirm = confirm.clone();
                raw.stay_open = stay_open.then_some(true);
            }
            ItemKind::Toggle { bind } => raw.bind = Some(bind.clone()),
            ItemKind::Choice {
                bind,
                options,
                present,
            } => {
                raw.bind = Some(bind.clone());
                raw.options = Some(options.clone());
                raw.present = (*present != Present::Inline).then_some(*present);
            }
            ItemKind::Stepper {
                bind,
                min,
                max,
                step,
                unit,
            } => {
                raw.bind = Some(bind.clone());
                raw.min = Some(*min);
                raw.max = Some(*max);
                raw.step = Some(*step);
                raw.unit = unit.clone();
            }
            ItemKind::Info { value, bind } => {
                raw.value = value.clone();
                raw.bind = bind.clone();
            }
            ItemKind::Slider {
                bind,
                min,
                max,
                step,
            } => {
                raw.bind = Some(bind.clone());
                raw.min = Some(*min);
                raw.max = Some(*max);
                raw.step = Some(*step);
            }
            ItemKind::Text {
                bind,
                purpose,
                max_chars,
                secret,
            } => {
                raw.bind = Some(bind.clone());
                raw.purpose = (*purpose != TextPurpose::Normal).then_some(*purpose);
                raw.max_chars = Some(*max_chars);
                raw.secret = secret.then_some(true);
            }
            ItemKind::Link { page } => raw.page = Some(page.clone()),
            ItemKind::Progress {
                bind,
                indeterminate,
            } => {
                raw.bind = Some(bind.clone());
                raw.indeterminate = indeterminate.then_some(true);
            }
            ItemKind::Binding { context, action } => {
                raw.context = Some(context.clone());
                raw.action = Some(action.clone());
            }
            ItemKind::DateTime { bind, fields } => {
                raw.bind = Some(bind.clone());
                raw.fields = Some(*fields);
            }
            ItemKind::Custom { renderer } => raw.renderer = Some(renderer.clone()),
        }
        raw
    }
}

impl Serialize for Item {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        RawItem::from(self).serialize(s)
    }
}

impl<'de> Deserialize<'de> for Item {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Item::try_from(RawItem::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
