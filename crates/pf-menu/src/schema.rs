//! Menu validation: structure (ids, links, tabs, ranges) and the bound value schema.
//!
//! A bound item may only offer values its schema accepts: a toggle binds a bool, a stepper or
//! slider range lies inside the scalar range, choice options are enum variants (or integers in
//! range). Together with the engine's clamping, the UI cannot produce an invalid value.

use crate::model::{Bind, Item, ItemId, ItemKind, Menu, PageId, FORMAT_V1};
use crate::value::MenuValue;
use pf_prefs::{PrefKind, PrefValue};
use std::collections::BTreeSet;
use std::fmt;

/// Upper bound on choice options (the SDK allows 16 in a picker; system pages stay within).
pub const MAX_CHOICE_OPTIONS: usize = 64;

/// The value constraint behind a [`Bind`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValueSchema {
    Bool,
    Integer { min: i64, max: i64 },
    Enum { variants: Vec<String> },
}

impl ValueSchema {
    pub fn from_pref_kind(kind: PrefKind) -> Self {
        match kind {
            PrefKind::Bool => ValueSchema::Bool,
            PrefKind::Scalar { min, max } => ValueSchema::Integer { min, max },
            PrefKind::Enum { variants } => ValueSchema::Enum {
                variants: variants.iter().map(|v| v.to_string()).collect(),
            },
        }
    }
}

/// What a schema source knows about a binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchemaLookup {
    Known(ValueSchema),
    /// The source owns this namespace and the key does not exist.
    Unknown,
    /// The source has no schema for this binding (host values); the item's own constraints
    /// are the only ones.
    Unconstrained,
}

/// Maps bindings to their value schema.
pub trait BindSchemas {
    fn lookup(&self, bind: &Bind) -> SchemaLookup;
}

/// The pf-prefs schema for `Bind::Pref`; other bindings are unconstrained.
#[derive(Clone, Copy, Debug, Default)]
pub struct PrefsSchemas;

impl BindSchemas for PrefsSchemas {
    fn lookup(&self, bind: &Bind) -> SchemaLookup {
        match bind {
            Bind::Pref(key) => match pf_prefs::spec(key) {
                Some(spec) => SchemaLookup::Known(ValueSchema::from_pref_kind(spec.kind)),
                None => SchemaLookup::Unknown,
            },
            Bind::AppOption(_) | Bind::Host(_) => SchemaLookup::Unconstrained,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MenuError {
    Format(String),
    DuplicatePage(PageId),
    DuplicateItem(ItemId),
    MissingRoot(PageId),
    MissingPage { item: ItemId, page: PageId },
    Tabs { page: PageId, reason: String },
    Primary { page: PageId, reason: String },
    Item { item: ItemId, reason: String },
    UnknownBind { item: ItemId, bind: Bind },
    BindMismatch { item: ItemId, reason: String },
    TooManyItems(usize),
}

impl fmt::Display for MenuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MenuError::Format(found) => {
                write!(f, "format is {found:?}, expected {FORMAT_V1:?}")
            }
            MenuError::DuplicatePage(p) => write!(f, "page {p} is declared twice"),
            MenuError::DuplicateItem(i) => write!(f, "item {i} is declared twice"),
            MenuError::MissingRoot(p) => write!(f, "root page {p} does not exist"),
            MenuError::MissingPage { item, page } => {
                write!(f, "item {item} links to missing page {page}")
            }
            MenuError::Tabs { page, reason } => write!(f, "page {page} tabs: {reason}"),
            MenuError::Primary { page, reason } => write!(f, "page {page} primary: {reason}"),
            MenuError::Item { item, reason } => write!(f, "item {item}: {reason}"),
            MenuError::UnknownBind { item, bind } => {
                write!(f, "item {item} binds unknown key {bind:?}")
            }
            MenuError::BindMismatch { item, reason } => write!(f, "item {item}: {reason}"),
            MenuError::TooManyItems(n) => write!(f, "{n} items exceed the engine limit"),
        }
    }
}

impl std::error::Error for MenuError {}

/// The engine indexes items and pages with u32 and choice options with u16.
pub const MAX_ITEMS: usize = 1 << 20;

impl Menu {
    /// Check structure and every binding against `schemas`.
    pub fn validate(&self, schemas: &dyn BindSchemas) -> Result<(), MenuError> {
        if self.format != FORMAT_V1 {
            return Err(MenuError::Format(self.format.clone()));
        }
        let mut pages = BTreeSet::new();
        for page in &self.pages {
            if !pages.insert(&page.id) {
                return Err(MenuError::DuplicatePage(page.id.clone()));
            }
        }
        if !pages.contains(&self.root) {
            return Err(MenuError::MissingRoot(self.root.clone()));
        }
        let mut items = BTreeSet::new();
        let mut count = 0usize;
        for item in self.items() {
            count += 1;
            if !items.insert(&item.id) {
                return Err(MenuError::DuplicateItem(item.id.clone()));
            }
        }
        if count > MAX_ITEMS {
            return Err(MenuError::TooManyItems(count));
        }
        for page in &self.pages {
            if !page.tabs.is_empty() {
                let tabs = |reason: &str| MenuError::Tabs {
                    page: page.id.clone(),
                    reason: reason.to_string(),
                };
                if !page.tabs.contains(&page.id) {
                    return Err(tabs("the group must include the page itself"));
                }
                let unique: BTreeSet<_> = page.tabs.iter().collect();
                if unique.len() != page.tabs.len() {
                    return Err(tabs("a page is listed twice"));
                }
                for sibling in &page.tabs {
                    match self.page(sibling) {
                        None => return Err(tabs(&format!("missing page {sibling}"))),
                        Some(other) if other.tabs != page.tabs => {
                            return Err(tabs(&format!("{sibling} lists a different group")))
                        }
                        Some(_) => {}
                    }
                }
            }
            if let Some(primary) = &page.primary {
                let found = page
                    .sections
                    .iter()
                    .flat_map(|s| s.items.iter())
                    .find(|item| &item.id == primary);
                match found {
                    None => {
                        return Err(MenuError::Primary {
                            page: page.id.clone(),
                            reason: format!("{primary} is not on this page"),
                        })
                    }
                    Some(item) if !matches!(item.kind, ItemKind::Action { .. }) => {
                        return Err(MenuError::Primary {
                            page: page.id.clone(),
                            reason: format!("{primary} is not an action"),
                        })
                    }
                    Some(_) => {}
                }
            }
            for item in page.sections.iter().flat_map(|s| s.items.iter()) {
                if let ItemKind::Link { page: target } = &item.kind {
                    if !pages.contains(target) {
                        return Err(MenuError::MissingPage {
                            item: item.id.clone(),
                            page: target.clone(),
                        });
                    }
                }
                check_item(item)?;
                check_bind(item, schemas)?;
            }
        }
        Ok(())
    }
}

fn item_err(item: &Item, reason: impl Into<String>) -> MenuError {
    MenuError::Item {
        item: item.id.clone(),
        reason: reason.into(),
    }
}

fn check_range(item: &Item, min: i64, max: i64, step: i64) -> Result<(), MenuError> {
    if min > max {
        return Err(item_err(item, format!("min {min} > max {max}")));
    }
    if step <= 0 {
        return Err(item_err(item, format!("step {step} must be positive")));
    }
    Ok(())
}

fn check_item(item: &Item) -> Result<(), MenuError> {
    match &item.kind {
        ItemKind::Choice { options, .. } => {
            if options.is_empty() {
                return Err(item_err(item, "a choice needs at least one option"));
            }
            if options.len() > MAX_CHOICE_OPTIONS {
                return Err(item_err(
                    item,
                    format!("{} options exceed {MAX_CHOICE_OPTIONS}", options.len()),
                ));
            }
            let unique: BTreeSet<_> = options.iter().map(|o| &o.value).collect();
            if unique.len() != options.len() {
                return Err(item_err(item, "option values must be unique"));
            }
        }
        ItemKind::Stepper { min, max, step, .. } | ItemKind::Slider { min, max, step, .. } => {
            check_range(item, *min, *max, *step)?
        }
        ItemKind::Text { max_chars, .. } if *max_chars == 0 => {
            return Err(item_err(item, "max_chars must be positive"))
        }
        _ => {}
    }
    Ok(())
}

fn mismatch(item: &Item, reason: String) -> MenuError {
    MenuError::BindMismatch {
        item: item.id.clone(),
        reason,
    }
}

fn check_bind(item: &Item, schemas: &dyn BindSchemas) -> Result<(), MenuError> {
    let Some(bind) = item.kind.bind() else {
        return Ok(());
    };
    let schema = match schemas.lookup(bind) {
        SchemaLookup::Known(schema) => schema,
        SchemaLookup::Unknown => {
            return Err(MenuError::UnknownBind {
                item: item.id.clone(),
                bind: bind.clone(),
            })
        }
        SchemaLookup::Unconstrained => return Ok(()),
    };
    let kind = item.kind.id();
    match (&item.kind, &schema) {
        (ItemKind::Toggle { .. }, ValueSchema::Bool) => Ok(()),
        (ItemKind::Stepper { min, max, .. }, ValueSchema::Integer { min: lo, max: hi })
        | (ItemKind::Slider { min, max, .. }, ValueSchema::Integer { min: lo, max: hi }) => {
            if min < lo || max > hi {
                Err(mismatch(
                    item,
                    format!("range {min}..={max} is outside the bound range {lo}..={hi}"),
                ))
            } else {
                Ok(())
            }
        }
        (ItemKind::Choice { options, .. }, ValueSchema::Enum { variants }) => {
            match options.iter().find(|o| !variants.contains(&o.value)) {
                Some(bad) => Err(mismatch(
                    item,
                    format!("option {:?} is not one of {variants:?}", bad.value),
                )),
                None => Ok(()),
            }
        }
        (ItemKind::Choice { options, .. }, ValueSchema::Integer { min, max }) => {
            for option in options {
                match option.value.parse::<i64>() {
                    Ok(n) if (*min..=*max).contains(&n) => {}
                    _ => {
                        return Err(mismatch(
                            item,
                            format!(
                                "option {:?} is not an integer in {min}..={max}",
                                option.value
                            ),
                        ))
                    }
                }
            }
            Ok(())
        }
        (ItemKind::Progress { .. }, ValueSchema::Integer { .. }) => Ok(()),
        (ItemKind::Info { .. }, _) => Ok(()),
        (_, schema) => Err(mismatch(
            item,
            format!("a {kind} item cannot bind a {schema:?} value"),
        )),
    }
}

impl Item {
    /// Whether `value` is a value this item can hold.
    pub fn accepts(&self, value: MenuValue) -> bool {
        match (&self.kind, value) {
            (ItemKind::Toggle { .. }, MenuValue::Bool(_)) => true,
            (ItemKind::Choice { options, .. }, MenuValue::Choice(i)) => {
                usize::from(i) < options.len()
            }
            (ItemKind::Stepper { min, max, .. }, MenuValue::Int(n))
            | (ItemKind::Slider { min, max, .. }, MenuValue::Int(n)) => (*min..=*max).contains(&n),
            (ItemKind::Progress { .. }, MenuValue::Int(n)) => (0..=100).contains(&n),
            (ItemKind::Info { bind: Some(_), .. }, _) => true,
            _ => false,
        }
    }

    /// The menu value for a stored preference value, if this item can show it.
    pub fn value_from_pref(&self, pref: PrefValue) -> Option<MenuValue> {
        let value = match (&self.kind, pref) {
            (ItemKind::Choice { options, .. }, PrefValue::Enum(v)) => {
                MenuValue::Choice(options.iter().position(|o| o.value == v)? as u16)
            }
            (ItemKind::Choice { options, .. }, PrefValue::Scalar(n)) => MenuValue::Choice(
                options
                    .iter()
                    .position(|o| o.value.parse::<i64>().ok() == Some(n))? as u16,
            ),
            (_, PrefValue::Bool(b)) => MenuValue::Bool(b),
            (_, PrefValue::Scalar(n)) => MenuValue::Int(n),
            (_, PrefValue::Enum(_)) => return None,
        };
        self.accepts(value).then_some(value)
    }

    /// The preference value to write for a menu value, validated by the key's `PrefSpec`.
    pub fn pref_value(&self, value: MenuValue) -> Result<PrefValue, MenuError> {
        let Some(Bind::Pref(key)) = self.kind.bind() else {
            return Err(item_err(self, "not bound to a preference"));
        };
        let spec = pf_prefs::spec(key).ok_or_else(|| MenuError::UnknownBind {
            item: self.id.clone(),
            bind: Bind::Pref(key.clone()),
        })?;
        if !self.accepts(value) {
            return Err(item_err(
                self,
                format!("{value:?} is not a value of this item"),
            ));
        }
        let candidate = match (value, spec.kind) {
            (MenuValue::Bool(b), _) => PrefValue::Bool(b),
            (MenuValue::Int(n), _) => PrefValue::Scalar(n),
            (MenuValue::Choice(i), kind) => {
                let ItemKind::Choice { options, .. } = &self.kind else {
                    return Err(item_err(self, "choice value on a non-choice item"));
                };
                let text = &options[usize::from(i)].value;
                match kind {
                    PrefKind::Enum { variants } => PrefValue::Enum(
                        variants
                            .iter()
                            .copied()
                            .find(|v| v == text)
                            .ok_or_else(|| mismatch(self, format!("{text:?} is not a variant")))?,
                    ),
                    _ => PrefValue::Scalar(
                        text.parse()
                            .map_err(|_| mismatch(self, format!("{text:?} is not an integer")))?,
                    ),
                }
            }
        };
        spec.validate(candidate)
            .map_err(|e| mismatch(self, e.to_string()))
    }
}
