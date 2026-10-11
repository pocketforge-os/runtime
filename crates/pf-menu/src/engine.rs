//! The focus engine: a reducer from [`MenuInput`] to [`MenuEffect`]s.
//!
//! Focus is 1-D within the current page (sections flattened). Disabled items are focusable
//! but inert, so focus never jumps over a greyed row. Up/Down wrap at the ends with a Boundary
//! cue; each page remembers its focus; links push pages, B pops and closes at the root; L1/R1
//! switch between the pages of a tab group; Left/Right adjust a value in place; A acts; Start
//! runs the page's primary action.
//!
//! The input path does not allocate: effects go into a buffer reserved at construction and
//! refer to items by [`ItemIndex`]; hosts read labels and bindings from [`MenuEngine::item`].

use crate::kind::ItemKindId;
use crate::model::{Item, ItemId, ItemKind, Menu, Page, PageId};
use crate::schema::{BindSchemas, MenuError};
use crate::value::MenuValue;
use pf_ports::{
    EffectiveBinding, GlyphError, GlyphResolver, GlyphResult, MonotonicTime, ShellAction,
};
use pf_scene::AxisMove;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

/// An item's position in the menu, counting every item of every page in document order.
/// Valid for one [`MenuEngine::revision`]; [`MenuEngine::edit`] renumbers.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ItemIndex(pub u32);

/// A page's position in `Menu::pages`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PageIndex(pub u32);

/// Engine input. There is deliberately no hold input in v1 (X11 is superseded by the Capture
/// sub-panel; review adjudication round 7, item 8).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MenuInput {
    Up,
    Down,
    Left,
    Right,
    /// A.
    Activate,
    /// B.
    Back,
    /// Start: the page's primary action.
    Primary,
    /// L1.
    TabPrev,
    /// R1.
    TabNext,
    PageUp,
    PageDown,
    /// Pointer or touch over an item: focus it.
    PointerHover(ItemIndex),
    /// Pointer click or touch tap: focus it and act.
    PointerActivate(ItemIndex),
    /// One-switch scanning: advance.
    ScanStep,
    /// One-switch scanning: act.
    ScanSelect,
    /// The host's answer to a [`MenuEffect::SetValue`], or a value change from elsewhere.
    Applied {
        item: ItemIndex,
        result: ApplyResult,
    },
    /// Time passed; expires pending values.
    Tick,
}

impl MenuInput {
    /// Map a shell action. `Custom("Start")` is the primary action; other custom actions are
    /// not menu input.
    pub fn from_shell_action(action: &ShellAction) -> Option<Self> {
        Some(match action {
            ShellAction::Move(AxisMove::Up) => MenuInput::Up,
            ShellAction::Move(AxisMove::Down) => MenuInput::Down,
            ShellAction::Move(AxisMove::Left) => MenuInput::Left,
            ShellAction::Move(AxisMove::Right) => MenuInput::Right,
            ShellAction::Activate => MenuInput::Activate,
            ShellAction::Back => MenuInput::Back,
            ShellAction::Custom(name) if name == "Start" => MenuInput::Primary,
            ShellAction::Custom(_) => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyResult {
    /// The value the backend now holds (the echo).
    Committed(MenuValue),
    /// The backend refused the write.
    Rejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RevertReason {
    /// No echo before the deadline ("X didn't respond").
    Timeout,
    Rejected,
    /// The echoed value is not a value this item can hold.
    InvalidValue,
}

/// Where an item's value is in the apply lifecycle. The new value is shown optimistically from
/// the moment the engine emits `SetValue`, while the state is `Pending`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ApplyState {
    #[default]
    Idle,
    Pending {
        deadline: MonotonicTime,
    },
    Committed,
    Reverted(RevertReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Feedback {
    Move,
    Boundary,
    Activate,
    Back,
    Toggle,
    Error,
    Open,
    Close,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnnounceWhat {
    Focus,
    Value,
    Page,
    Overlay,
    Reverted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MenuEffect {
    /// Write `value` to the item's binding, then answer with [`MenuInput::Applied`].
    SetValue {
        item: ItemIndex,
        value: MenuValue,
    },
    /// Run the item (action, binding capture, date/time editor, custom).
    Invoke {
        item: ItemIndex,
    },
    /// Open the system keyboard for a text item.
    OpenKeyboard {
        item: ItemIndex,
    },
    /// Close the whole menu.
    Close,
    FocusChanged {
        item: Option<ItemIndex>,
    },
    PageChanged {
        page: PageIndex,
    },
    OverlayChanged {
        overlay: Option<Overlay>,
    },
    Committed {
        item: ItemIndex,
    },
    Reverted {
        item: ItemIndex,
        reason: RevertReason,
    },
    Feedback(Feedback),
    Announce {
        item: Option<ItemIndex>,
        what: AnnounceWhat,
    },
    Redraw,
}

/// A layer above the page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Overlay {
    /// Confirm card for an action. Focus starts on Cancel (`confirm_focused == false`).
    Confirm {
        item: ItemIndex,
        confirm_focused: bool,
    },
    /// Option picker for a choice.
    Picker { item: ItemIndex, cursor: u16 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegendAction {
    Activate,
    Back,
    /// Left/Right.
    Adjust,
    /// L1/R1.
    Tabs,
    /// Start.
    Primary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegendLabel {
    Select,
    Toggle,
    Change,
    Choose,
    Edit,
    Open,
    Back,
    Close,
    Cancel,
    Confirm,
    Tabs,
    /// The page's primary action; its label is the item's label.
    Primary(ItemIndex),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LegendEntry {
    pub action: LegendAction,
    pub label: LegendLabel,
}

/// A legend entry with the bindings that trigger it on the active device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegendGlyph {
    pub entry: LegendEntry,
    pub bindings: Vec<EffectiveBinding>,
}

const MAX_LEGEND: usize = 5;
const NO_LEGEND: LegendEntry = LegendEntry {
    action: LegendAction::Back,
    label: LegendLabel::Close,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineOptions {
    /// How long a written value may wait for its echo (the system menu's 1 s rule).
    pub echo_timeout: Duration,
    /// Rows moved by PageUp/PageDown.
    pub page_jump: u32,
    /// Left/Right on an item that cannot be adjusted switch tabs (4:3 single-pane layout).
    pub dpad_switches_tabs: bool,
}

impl Default for EngineOptions {
    fn default() -> Self {
        EngineOptions {
            echo_timeout: Duration::from_secs(1),
            page_jump: 5,
            dpad_switches_tabs: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Slot {
    value: Option<MenuValue>,
    /// The value to restore if the pending write fails.
    previous: Option<MenuValue>,
    state: ApplyState,
}

/// Where a remembered focus was: the item, and its visible position as a fallback.
#[derive(Clone, Copy, Debug)]
struct Memory {
    item: u32,
    pos: u32,
}

/// Precomputed lookups for one menu revision.
#[derive(Debug, Default)]
struct Index {
    /// (page, section, item) for each flattened item.
    loc: Vec<(u32, u32, u32)>,
    /// Flattened item range of each page.
    page_items: Vec<(u32, u32)>,
    /// Tab group of each page, as page indices.
    tabs: Vec<Vec<u32>>,
    primary: Vec<Option<u32>>,
    /// Link target page of each item.
    link: Vec<Option<u32>>,
    root: u32,
}

impl Index {
    fn build(menu: &Menu) -> Self {
        let page_of: BTreeMap<&PageId, u32> = menu
            .pages
            .iter()
            .enumerate()
            .map(|(i, p)| (&p.id, i as u32))
            .collect();
        let mut index = Index {
            root: page_of[&menu.root],
            ..Index::default()
        };
        for (p, page) in menu.pages.iter().enumerate() {
            let start = index.loc.len() as u32;
            for (s, section) in page.sections.iter().enumerate() {
                for (i, item) in section.items.iter().enumerate() {
                    if page.primary.as_ref() == Some(&item.id) {
                        index.primary.resize(p + 1, None);
                        index.primary[p] = Some(index.loc.len() as u32);
                    }
                    index.loc.push((p as u32, s as u32, i as u32));
                    index.link.push(match &item.kind {
                        ItemKind::Link { page } => Some(page_of[page]),
                        _ => None,
                    });
                }
            }
            index.page_items.push((start, index.loc.len() as u32));
            index
                .tabs
                .push(page.tabs.iter().map(|id| page_of[id]).collect());
        }
        index.primary.resize(menu.pages.len(), None);
        index
    }
}

/// The menu engine. See the module documentation.
pub struct MenuEngine {
    menu: Menu,
    options: EngineOptions,
    index: Index,
    slots: Vec<Slot>,
    pending: usize,
    /// Every predicate the menu references, sorted by name.
    flags: Vec<(String, bool)>,
    stack: Vec<u32>,
    focus: Option<u32>,
    focus_pos: u32,
    memory: Vec<Option<Memory>>,
    overlay: Option<Overlay>,
    effects: Vec<MenuEffect>,
    legend: [LegendEntry; MAX_LEGEND],
    legend_len: usize,
    revision: u64,
}

impl MenuEngine {
    pub fn new(
        menu: Menu,
        schemas: &dyn BindSchemas,
        options: EngineOptions,
    ) -> Result<Self, MenuError> {
        menu.validate(schemas)?;
        let mut engine = MenuEngine {
            menu,
            options,
            index: Index::default(),
            slots: Vec::new(),
            pending: 0,
            flags: Vec::new(),
            stack: Vec::new(),
            focus: None,
            focus_pos: 0,
            memory: Vec::new(),
            overlay: None,
            effects: Vec::new(),
            legend: [NO_LEGEND; MAX_LEGEND],
            legend_len: 0,
            revision: 0,
        };
        engine.reindex();
        engine.slots = vec![Slot::default(); engine.index.loc.len()];
        engine.memory = vec![None; engine.menu.pages.len()];
        engine.stack.push(engine.index.root);
        engine.enter_page();
        engine.refresh_legend();
        Ok(engine)
    }

    fn reindex(&mut self) {
        self.index = Index::build(&self.menu);
        let mut names = BTreeSet::new();
        for item in self.menu.items() {
            item.visible.predicates(&mut names);
            item.enabled.predicates(&mut names);
        }
        let old: BTreeMap<String, bool> = self.flags.drain(..).collect();
        self.flags = names
            .into_iter()
            .map(|n| (n.to_string(), old.get(n).copied().unwrap_or(false)))
            .collect();
        // Worst case per input: three effects per expiring pending value plus a bounded
        // handful from the handler and the state diff.
        let need = 16 + 3 * self.index.loc.len();
        self.effects = Vec::with_capacity(need);
        self.stack.reserve(self.menu.pages.len() + 1);
        self.revision += 1;
    }

    // ---- read API ----------------------------------------------------------------------

    pub fn menu(&self) -> &Menu {
        &self.menu
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn item_count(&self) -> usize {
        self.index.loc.len()
    }

    pub fn item(&self, idx: ItemIndex) -> &Item {
        let (p, s, i) = self.index.loc[idx.0 as usize];
        &self.menu.pages[p as usize].sections[s as usize].items[i as usize]
    }

    pub fn item_index(&self, id: &ItemId) -> Option<ItemIndex> {
        (0..self.index.loc.len() as u32)
            .map(ItemIndex)
            .find(|&idx| &self.item(idx).id == id)
    }

    pub fn page_index(&self, id: &PageId) -> Option<PageIndex> {
        self.menu
            .pages
            .iter()
            .position(|p| &p.id == id)
            .map(|p| PageIndex(p as u32))
    }

    /// The page an item is on.
    pub fn page_of(&self, idx: ItemIndex) -> PageIndex {
        PageIndex(self.index.loc[idx.0 as usize].0)
    }

    pub fn page(&self) -> PageIndex {
        PageIndex(self.top())
    }

    pub fn page_ref(&self, page: PageIndex) -> &Page {
        &self.menu.pages[page.0 as usize]
    }

    /// Pages on the stack (1 at the root).
    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    pub fn focus(&self) -> Option<ItemIndex> {
        self.focus.map(ItemIndex)
    }

    pub fn overlay(&self) -> Option<Overlay> {
        self.overlay
    }

    pub fn value(&self, idx: ItemIndex) -> Option<MenuValue> {
        self.slots[idx.0 as usize].value
    }

    pub fn apply_state(&self, idx: ItemIndex) -> ApplyState {
        self.slots[idx.0 as usize].state
    }

    pub fn is_visible(&self, idx: ItemIndex) -> bool {
        self.visible(idx.0)
    }

    pub fn is_enabled(&self, idx: ItemIndex) -> bool {
        self.enabled(idx.0)
    }

    /// The visible items of a page, in order.
    pub fn visible_items(&self, page: PageIndex) -> impl Iterator<Item = ItemIndex> + '_ {
        let (start, end) = self.index.page_items[page.0 as usize];
        (start..end).filter(|&i| self.visible(i)).map(ItemIndex)
    }

    pub fn flag(&self, name: &str) -> bool {
        self.flags
            .binary_search_by(|(k, _)| k.as_str().cmp(name))
            .map(|at| self.flags[at].1)
            .unwrap_or(false)
    }

    /// The prompt-bar entries for the current state.
    pub fn legend(&self) -> &[LegendEntry] {
        &self.legend[..self.legend_len]
    }

    /// The legend with the active device's bindings (off the input path; allocates).
    pub fn legend_with_glyphs(
        &self,
        glyphs: &dyn GlyphResolver,
    ) -> Result<Vec<LegendGlyph>, GlyphError> {
        let mut out = Vec::with_capacity(self.legend_len);
        for entry in self.legend() {
            let actions: &[ShellAction] = match entry.action {
                LegendAction::Activate => &[ShellAction::Activate],
                LegendAction::Back => &[ShellAction::Back],
                LegendAction::Adjust => &[
                    ShellAction::Move(AxisMove::Left),
                    ShellAction::Move(AxisMove::Right),
                ],
                // L1/R1 are not shell-contract actions yet; the host draws them.
                LegendAction::Tabs => &[],
                LegendAction::Primary => &[],
            };
            let mut bindings = Vec::new();
            for action in actions {
                if let GlyphResult::Resolved(binding) = glyphs.resolve(action)? {
                    bindings.push(binding);
                }
            }
            if entry.action == LegendAction::Primary {
                if let GlyphResult::Resolved(binding) =
                    glyphs.resolve(&ShellAction::Custom("Start".to_string()))?
                {
                    bindings.push(binding);
                }
            }
            out.push(LegendGlyph {
                entry: *entry,
                bindings,
            });
        }
        Ok(out)
    }

    // ---- host API (not per input) ------------------------------------------------------

    /// Set a predicate. Returns false if no item references it. Focus moves off an item that
    /// becomes hidden; an overlay on it closes.
    pub fn set_flag(&mut self, name: &str, value: bool) -> bool {
        let Ok(at) = self.flags.binary_search_by(|(k, _)| k.as_str().cmp(name)) else {
            return false;
        };
        self.flags[at].1 = value;
        self.settle();
        self.refresh_legend();
        true
    }

    /// Seed or overwrite a value from the backend (no lifecycle, no effects).
    pub fn set_value(&mut self, idx: ItemIndex, value: MenuValue) -> Result<(), MenuError> {
        let item = self.item(idx);
        if !item.accepts(value) {
            return Err(MenuError::Item {
                item: item.id.clone(),
                reason: format!("{value:?} is not a value of this item"),
            });
        }
        let slot = &mut self.slots[idx.0 as usize];
        if matches!(slot.state, ApplyState::Pending { .. }) {
            self.pending -= 1;
        }
        *slot = Slot {
            value: Some(value),
            previous: None,
            state: ApplyState::Idle,
        };
        Ok(())
    }

    /// Back to the root page with no overlay (when the host reopens the menu).
    pub fn reset(&mut self) {
        self.leave_page();
        self.stack.clear();
        self.stack.push(self.index.root);
        self.overlay = None;
        self.enter_page();
        self.refresh_legend();
    }

    /// Edit the model. Values, focus memory and the page stack follow item and page ids; a
    /// focused item that disappears hands focus to its nearest visible neighbour.
    pub fn edit(
        &mut self,
        schemas: &dyn BindSchemas,
        edit: impl FnOnce(&mut Menu),
    ) -> Result<(), MenuError> {
        let mut next = self.menu.clone();
        edit(&mut next);
        next.validate(schemas)?;
        self.leave_page();

        let item_id = |engine: &Self, i: u32| engine.item(ItemIndex(i)).id.clone();
        let slots: BTreeMap<ItemId, Slot> = (0..self.slots.len() as u32)
            .map(|i| (item_id(self, i), self.slots[i as usize]))
            .collect();
        let memory: BTreeMap<PageId, (Option<ItemId>, u32)> = self
            .memory
            .iter()
            .enumerate()
            .filter_map(|(p, m)| {
                m.map(|m| {
                    let id = (m.item < self.slots.len() as u32).then(|| item_id(self, m.item));
                    (self.menu.pages[p].id.clone(), (id, m.pos))
                })
            })
            .collect();
        let stack: Vec<PageId> = self
            .stack
            .iter()
            .map(|&p| self.menu.pages[p as usize].id.clone())
            .collect();
        let focus = self.focus.map(|f| item_id(self, f));
        let overlay = self.overlay.map(|o| match o {
            Overlay::Confirm {
                item,
                confirm_focused,
            } => (item_id(self, item.0), Some(confirm_focused), 0),
            Overlay::Picker { item, cursor } => (item_id(self, item.0), None, cursor),
        });

        self.menu = next;
        self.reindex();
        let find = |engine: &Self, id: &ItemId| engine.item_index(id).map(|i| i.0);
        self.slots = (0..self.index.loc.len() as u32)
            .map(|i| slots.get(&item_id(self, i)).copied().unwrap_or_default())
            .collect();
        for i in 0..self.slots.len() {
            let item = self.item(ItemIndex(i as u32));
            let slot = self.slots[i];
            if slot.value.is_some_and(|v| !item.accepts(v)) {
                self.slots[i] = Slot::default();
            }
        }
        self.pending = self
            .slots
            .iter()
            .filter(|s| matches!(s.state, ApplyState::Pending { .. }))
            .count();
        self.memory = self
            .menu
            .pages
            .iter()
            .map(|page| {
                memory.get(&page.id).map(|(id, pos)| Memory {
                    item: id
                        .as_ref()
                        .and_then(|id| find(self, id))
                        .unwrap_or(u32::MAX),
                    pos: *pos,
                })
            })
            .collect();
        self.stack = stack
            .iter()
            .filter_map(|id| self.page_index(id).map(|p| p.0))
            .collect();
        if self.stack.first() != Some(&self.index.root) {
            self.stack.insert(0, self.index.root);
        }
        self.stack.reserve(self.menu.pages.len() + 1);
        self.focus = focus.and_then(|id| find(self, &id));
        if self.focus.is_none() {
            // Fall back to the remembered position on the (possibly new) top page.
            self.focus_pos = self.memory[self.top() as usize].map_or(0, |m| m.pos);
        }
        self.overlay = overlay.and_then(|(id, confirm, cursor)| {
            let item = ItemIndex(find(self, &id)?);
            Some(match confirm {
                Some(confirm_focused) => Overlay::Confirm {
                    item,
                    confirm_focused,
                },
                None => Overlay::Picker { item, cursor },
            })
        });
        self.settle();
        self.refresh_legend();
        Ok(())
    }

    // ---- the reducer -------------------------------------------------------------------

    /// Apply one input at time `now`. The returned effects are valid until the next call.
    pub fn input(&mut self, input: MenuInput, now: MonotonicTime) -> &[MenuEffect] {
        self.effects.clear();
        let before = (self.top(), self.focus, self.overlay);
        self.expire(now);
        match input {
            MenuInput::Up => self.step(-1, true),
            MenuInput::Down | MenuInput::ScanStep => self.step(1, true),
            MenuInput::PageUp => self.step(-(self.options.page_jump.max(1) as i64), false),
            MenuInput::PageDown => self.step(self.options.page_jump.max(1) as i64, false),
            MenuInput::Left => self.adjust(-1, now),
            MenuInput::Right => self.adjust(1, now),
            MenuInput::Activate | MenuInput::ScanSelect => self.activate(now),
            MenuInput::Back => self.back(),
            MenuInput::Primary => self.primary(now),
            MenuInput::TabPrev => self.tab(-1),
            MenuInput::TabNext => self.tab(1),
            MenuInput::PointerHover(idx) => {
                self.hover(idx);
            }
            MenuInput::PointerActivate(idx) => {
                if self.hover(idx) {
                    self.activate(now);
                }
            }
            MenuInput::Applied { item, result } => self.applied(item, result),
            MenuInput::Tick => {}
        }
        self.settle();
        let after = (self.top(), self.focus, self.overlay);
        if after.0 != before.0 {
            self.emit(MenuEffect::PageChanged {
                page: PageIndex(after.0),
            });
            self.emit(MenuEffect::Announce {
                item: None,
                what: AnnounceWhat::Page,
            });
        }
        if after.2 != before.2 {
            self.emit(MenuEffect::OverlayChanged { overlay: after.2 });
            if after.2.is_some() {
                self.emit(MenuEffect::Announce {
                    item: after.2.map(|o| match o {
                        Overlay::Confirm { item, .. } | Overlay::Picker { item, .. } => item,
                    }),
                    what: AnnounceWhat::Overlay,
                });
            }
        }
        if after.1 != before.1 {
            self.emit(MenuEffect::FocusChanged {
                item: self.focus(),
            });
            if self.focus.is_some() {
                self.emit(MenuEffect::Announce {
                    item: self.focus(),
                    what: AnnounceWhat::Focus,
                });
            }
        }
        if !self.effects.is_empty() {
            self.emit(MenuEffect::Redraw);
        }
        self.refresh_legend();
        &self.effects
    }

    // ---- internals ---------------------------------------------------------------------

    fn emit(&mut self, effect: MenuEffect) {
        debug_assert!(
            self.effects.len() < self.effects.capacity(),
            "effect buffer would grow"
        );
        self.effects.push(effect);
    }

    fn top(&self) -> u32 {
        *self.stack.last().expect("the page stack is never empty")
    }

    fn item_at(&self, i: u32) -> &Item {
        self.item(ItemIndex(i))
    }

    fn visible(&self, i: u32) -> bool {
        self.item_at(i).visible.eval(&|name| self.flag(name))
    }

    fn enabled(&self, i: u32) -> bool {
        self.item_at(i).enabled.eval(&|name| self.flag(name))
    }

    fn page_range(&self, page: u32) -> (u32, u32) {
        self.index.page_items[page as usize]
    }

    fn on_page(&self, page: u32, i: u32) -> bool {
        let (start, end) = self.page_range(page);
        (start..end).contains(&i)
    }

    fn visible_count(&self, page: u32) -> u32 {
        let (start, end) = self.page_range(page);
        (start..end).filter(|&i| self.visible(i)).count() as u32
    }

    fn nth_visible(&self, page: u32, n: u32) -> Option<u32> {
        let (start, end) = self.page_range(page);
        (start..end).filter(|&i| self.visible(i)).nth(n as usize)
    }

    fn visible_pos(&self, page: u32, i: u32) -> u32 {
        let (start, _) = self.page_range(page);
        (start..i).filter(|&j| self.visible(j)).count() as u32
    }

    /// Restore focus invariants: focus is a visible item of the top page, or none when the
    /// page has no visible item; an overlay belongs to the focused, visible, enabled item.
    fn settle(&mut self) {
        let page = self.top();
        match self.focus {
            Some(f) if self.on_page(page, f) && self.visible(f) => {
                self.focus_pos = self.visible_pos(page, f);
            }
            _ => {
                let count = self.visible_count(page);
                self.focus = if count == 0 {
                    None
                } else {
                    self.focus_pos = self.focus_pos.min(count - 1);
                    self.nth_visible(page, self.focus_pos)
                };
            }
        }
        if let Some(overlay) = self.overlay {
            let item = match overlay {
                Overlay::Confirm { item, .. } | Overlay::Picker { item, .. } => item.0,
            };
            if self.focus != Some(item) || !self.enabled(item) {
                self.overlay = None;
            }
        }
    }

    fn leave_page(&mut self) {
        let page = self.top() as usize;
        self.memory[page] = self.focus.map(|item| Memory {
            item,
            pos: self.focus_pos,
        });
    }

    fn enter_page(&mut self) {
        let page = self.top();
        match self.memory[page as usize] {
            Some(m) => {
                self.focus = Some(m.item).filter(|&i| self.on_page(page, i) && self.visible(i));
                self.focus_pos = m.pos;
            }
            None => {
                self.focus = None;
                self.focus_pos = 0;
            }
        }
        self.settle();
    }

    fn step(&mut self, delta: i64, wrap: bool) {
        if self.overlay.is_some() {
            return self.overlay_step(delta.signum());
        }
        let page = self.top();
        let count = self.visible_count(page) as i64;
        if count == 0 {
            return self.emit(MenuEffect::Feedback(Feedback::Boundary));
        }
        if self.focus.is_none() {
            self.focus = self.nth_visible(page, 0);
            self.focus_pos = 0;
            return self.emit(MenuEffect::Feedback(Feedback::Move));
        }
        let pos = self.focus_pos as i64;
        let target = pos + delta;
        let (next, feedback) = if (0..count).contains(&target) {
            (target, Feedback::Move)
        } else if wrap && count > 1 {
            (target.rem_euclid(count), Feedback::Boundary)
        } else {
            (target.clamp(0, count - 1), Feedback::Boundary)
        };
        self.focus_pos = next as u32;
        self.focus = self.nth_visible(page, self.focus_pos);
        self.emit(MenuEffect::Feedback(feedback));
    }

    fn overlay_step(&mut self, dir: i64) {
        match self.overlay {
            Some(Overlay::Picker { item, cursor }) => {
                let len = self.choice_len(item.0) as i64;
                let target = cursor as i64 + dir;
                let feedback = if (0..len).contains(&target) {
                    Feedback::Move
                } else {
                    Feedback::Boundary
                };
                self.overlay = Some(Overlay::Picker {
                    item,
                    cursor: target.rem_euclid(len) as u16,
                });
                self.emit(MenuEffect::Feedback(feedback));
            }
            Some(Overlay::Confirm {
                item,
                confirm_focused,
            }) => {
                let want = dir > 0;
                if want == confirm_focused {
                    return self.emit(MenuEffect::Feedback(Feedback::Boundary));
                }
                self.overlay = Some(Overlay::Confirm {
                    item,
                    confirm_focused: want,
                });
                self.emit(MenuEffect::Feedback(Feedback::Move));
            }
            None => {}
        }
    }

    fn choice_len(&self, i: u32) -> usize {
        match &self.item_at(i).kind {
            ItemKind::Choice { options, .. } => options.len(),
            _ => 0,
        }
    }

    fn adjustable(&self, i: u32) -> bool {
        matches!(
            self.item_at(i).kind,
            ItemKind::Toggle { .. }
                | ItemKind::Choice { .. }
                | ItemKind::Stepper { .. }
                | ItemKind::Slider { .. }
        )
    }

    fn adjust(&mut self, dir: i64, now: MonotonicTime) {
        if self.overlay.is_some() {
            return self.overlay_step(dir);
        }
        let Some(f) = self.focus.filter(|&f| self.adjustable(f)) else {
            if self.options.dpad_switches_tabs {
                self.tab(dir);
            }
            return;
        };
        if !self.enabled(f) {
            return self.emit(MenuEffect::Feedback(Feedback::Error));
        }
        let current = self.slots[f as usize].value;
        let next = match &self.item_at(f).kind {
            ItemKind::Toggle { .. } => {
                let on = matches!(current, Some(MenuValue::Bool(true)));
                Some((MenuValue::Bool(!on), Feedback::Toggle))
            }
            ItemKind::Choice { options, .. } => {
                let len = options.len() as i64;
                let at = match current {
                    Some(MenuValue::Choice(i)) => i as i64,
                    _ => 0,
                };
                let target = at + dir;
                match len {
                    1 => None,
                    _ if (0..len).contains(&target) => {
                        Some((MenuValue::Choice(target as u16), Feedback::Move))
                    }
                    _ => Some((
                        MenuValue::Choice(target.rem_euclid(len) as u16),
                        Feedback::Boundary,
                    )),
                }
            }
            ItemKind::Stepper {
                min, max, step, ..
            }
            | ItemKind::Slider {
                min, max, step, ..
            } => {
                let at = match current {
                    Some(MenuValue::Int(n)) => n,
                    _ => *min,
                };
                let target = at.saturating_add(dir * step).clamp(*min, *max);
                (target != at || current.is_none())
                    .then_some((MenuValue::Int(target), Feedback::Move))
            }
            _ => None,
        };
        match next {
            Some((value, feedback)) => {
                self.write(f, value, now);
                self.emit(MenuEffect::Feedback(feedback));
            }
            None => self.emit(MenuEffect::Feedback(Feedback::Boundary)),
        }
    }

    /// Show `value` at once and ask the host to write it.
    fn write(&mut self, i: u32, value: MenuValue, now: MonotonicTime) {
        let deadline = now.saturating_add(self.options.echo_timeout);
        let slot = &mut self.slots[i as usize];
        if !matches!(slot.state, ApplyState::Pending { .. }) {
            slot.previous = slot.value;
            self.pending += 1;
        }
        slot.value = Some(value);
        slot.state = ApplyState::Pending { deadline };
        let item = ItemIndex(i);
        self.emit(MenuEffect::SetValue { item, value });
        self.emit(MenuEffect::Announce {
            item: Some(item),
            what: AnnounceWhat::Value,
        });
    }

    fn revert(&mut self, i: u32, reason: RevertReason) {
        let slot = &mut self.slots[i as usize];
        if matches!(slot.state, ApplyState::Pending { .. }) {
            self.pending -= 1;
            slot.value = slot.previous;
        }
        slot.state = ApplyState::Reverted(reason);
        let item = ItemIndex(i);
        self.emit(MenuEffect::Reverted { item, reason });
        self.emit(MenuEffect::Feedback(Feedback::Error));
        self.emit(MenuEffect::Announce {
            item: Some(item),
            what: AnnounceWhat::Reverted,
        });
    }

    fn expire(&mut self, now: MonotonicTime) {
        if self.pending == 0 {
            return;
        }
        for i in 0..self.slots.len() {
            if let ApplyState::Pending { deadline } = self.slots[i].state {
                if deadline <= now {
                    self.revert(i as u32, RevertReason::Timeout);
                }
            }
        }
    }

    fn applied(&mut self, item: ItemIndex, result: ApplyResult) {
        let i = item.0;
        if i as usize >= self.slots.len() {
            return;
        }
        let pending = matches!(self.slots[i as usize].state, ApplyState::Pending { .. });
        match result {
            ApplyResult::Committed(value) if self.item_at(i).accepts(value) => {
                let slot = &mut self.slots[i as usize];
                let changed = slot.value != Some(value);
                slot.value = Some(value);
                if pending {
                    slot.state = ApplyState::Committed;
                    self.pending -= 1;
                    self.emit(MenuEffect::Committed { item });
                }
                if changed {
                    self.emit(MenuEffect::Announce {
                        item: Some(item),
                        what: AnnounceWhat::Value,
                    });
                }
            }
            ApplyResult::Committed(_) => self.revert(i, RevertReason::InvalidValue),
            ApplyResult::Rejected if pending => self.revert(i, RevertReason::Rejected),
            ApplyResult::Rejected => {}
        }
    }

    fn activate(&mut self, now: MonotonicTime) {
        match self.overlay {
            Some(Overlay::Confirm {
                item,
                confirm_focused,
            }) => {
                self.overlay = None;
                if confirm_focused {
                    self.invoke(item.0);
                } else {
                    self.emit(MenuEffect::Feedback(Feedback::Back));
                }
                return;
            }
            Some(Overlay::Picker { item, cursor }) => {
                self.overlay = None;
                let value = MenuValue::Choice(cursor);
                if self.slots[item.0 as usize].value != Some(value) {
                    self.write(item.0, value, now);
                }
                return self.emit(MenuEffect::Feedback(Feedback::Activate));
            }
            None => {}
        }
        if let Some(f) = self.focus {
            self.act_on(f, now);
        }
    }

    fn act_on(&mut self, f: u32, now: MonotonicTime) {
        if !self.enabled(f) {
            return self.emit(MenuEffect::Feedback(Feedback::Error));
        }
        let item = ItemIndex(f);
        let kind = &self.item_at(f).kind;
        let confirm = matches!(kind, ItemKind::Action { confirm: Some(_), .. });
        match kind.id() {
            ItemKindId::Action if confirm => {
                self.overlay = Some(Overlay::Confirm {
                    item,
                    confirm_focused: false,
                });
                self.emit(MenuEffect::Feedback(Feedback::Open));
            }
            ItemKindId::Action
            | ItemKindId::Binding
            | ItemKindId::DateTime
            | ItemKindId::Custom => self.invoke(f),
            ItemKindId::Toggle => {
                let on = matches!(self.slots[f as usize].value, Some(MenuValue::Bool(true)));
                self.write(f, MenuValue::Bool(!on), now);
                self.emit(MenuEffect::Feedback(Feedback::Toggle));
            }
            ItemKindId::Choice => {
                let cursor = match self.slots[f as usize].value {
                    Some(MenuValue::Choice(i)) => i,
                    _ => 0,
                };
                self.overlay = Some(Overlay::Picker { item, cursor });
                self.emit(MenuEffect::Feedback(Feedback::Open));
            }
            ItemKindId::Text => {
                self.emit(MenuEffect::OpenKeyboard { item });
                self.emit(MenuEffect::Feedback(Feedback::Open));
            }
            ItemKindId::Link => {
                let target = self.index.link[f as usize].expect("validated link");
                self.push(target);
            }
            ItemKindId::Info | ItemKindId::Progress | ItemKindId::Stepper | ItemKindId::Slider => {}
        }
    }

    fn invoke(&mut self, i: u32) {
        let stay_open = matches!(
            self.item_at(i).kind,
            ItemKind::Action {
                stay_open: true,
                ..
            }
        ) || !matches!(self.item_at(i).kind, ItemKind::Action { .. });
        self.emit(MenuEffect::Invoke { item: ItemIndex(i) });
        self.emit(MenuEffect::Feedback(Feedback::Activate));
        if !stay_open {
            self.emit(MenuEffect::Close);
        }
    }

    fn push(&mut self, target: u32) {
        self.leave_page();
        if let Some(at) = self.stack.iter().position(|&p| p == target) {
            self.stack.truncate(at + 1);
        } else {
            self.stack.push(target);
        }
        self.enter_page();
        self.emit(MenuEffect::Feedback(Feedback::Open));
    }

    fn back(&mut self) {
        if self.overlay.take().is_some() {
            return self.emit(MenuEffect::Feedback(Feedback::Back));
        }
        if self.stack.len() > 1 {
            self.leave_page();
            self.stack.pop();
            self.enter_page();
            return self.emit(MenuEffect::Feedback(Feedback::Back));
        }
        self.emit(MenuEffect::Feedback(Feedback::Close));
        self.emit(MenuEffect::Close);
    }

    fn primary(&mut self, now: MonotonicTime) {
        if self.overlay.is_some() {
            return;
        }
        let Some(p) = self.index.primary[self.top() as usize] else {
            return;
        };
        if self.visible(p) {
            self.act_on(p, now);
        }
    }

    fn tab(&mut self, dir: i64) {
        if self.overlay.is_some() {
            return;
        }
        let top = self.top();
        let group = &self.index.tabs[top as usize];
        let Some(at) = group.iter().position(|&p| p == top) else {
            return;
        };
        let target = at as i64 + dir;
        if !(0..group.len() as i64).contains(&target) {
            return self.emit(MenuEffect::Feedback(Feedback::Boundary));
        }
        let next = group[target as usize];
        self.leave_page();
        *self.stack.last_mut().expect("non-empty stack") = next;
        self.enter_page();
        self.emit(MenuEffect::Feedback(Feedback::Move));
    }

    /// Focus an item under the pointer. Returns whether the pointer is on a usable item.
    fn hover(&mut self, idx: ItemIndex) -> bool {
        let i = idx.0;
        if self.overlay.is_some()
            || i as usize >= self.slots.len()
            || !self.on_page(self.top(), i)
            || !self.visible(i)
        {
            return false;
        }
        self.focus = Some(i);
        self.focus_pos = self.visible_pos(self.top(), i);
        true
    }

    fn refresh_legend(&mut self) {
        let mut legend = [NO_LEGEND; MAX_LEGEND];
        let mut n = 0;
        let mut add = |action, label| {
            legend[n] = LegendEntry { action, label };
            n += 1;
        };
        match self.overlay {
            Some(Overlay::Confirm {
                confirm_focused, ..
            }) => {
                let label = if confirm_focused {
                    LegendLabel::Confirm
                } else {
                    LegendLabel::Cancel
                };
                add(LegendAction::Activate, label);
                add(LegendAction::Back, LegendLabel::Cancel);
            }
            Some(Overlay::Picker { .. }) => {
                add(LegendAction::Activate, LegendLabel::Choose);
                add(LegendAction::Back, LegendLabel::Cancel);
            }
            None => {
                if let Some(f) = self.focus.filter(|&f| self.enabled(f)) {
                    let (activate, adjust) = match &self.item_at(f).kind {
                        ItemKind::Action { confirm, .. } => (
                            Some(if confirm.is_some() {
                                LegendLabel::Open
                            } else {
                                LegendLabel::Select
                            }),
                            None,
                        ),
                        ItemKind::Toggle { .. } => {
                            (Some(LegendLabel::Toggle), Some(LegendLabel::Toggle))
                        }
                        ItemKind::Choice { .. } => {
                            (Some(LegendLabel::Choose), Some(LegendLabel::Change))
                        }
                        ItemKind::Stepper { .. } | ItemKind::Slider { .. } => {
                            (None, Some(LegendLabel::Change))
                        }
                        ItemKind::Text { .. } | ItemKind::Binding { .. } => {
                            (Some(LegendLabel::Edit), None)
                        }
                        ItemKind::DateTime { .. } => (Some(LegendLabel::Edit), None),
                        ItemKind::Link { .. } => (Some(LegendLabel::Open), None),
                        ItemKind::Custom { .. } => (Some(LegendLabel::Select), None),
                        ItemKind::Info { .. } | ItemKind::Progress { .. } => (None, None),
                    };
                    if let Some(label) = activate {
                        add(LegendAction::Activate, label);
                    }
                    if let Some(label) = adjust {
                        add(LegendAction::Adjust, label);
                    }
                }
                let back = if self.stack.len() > 1 {
                    LegendLabel::Back
                } else {
                    LegendLabel::Close
                };
                add(LegendAction::Back, back);
                let top = self.top();
                if self.index.tabs[top as usize].len() > 1 {
                    add(LegendAction::Tabs, LegendLabel::Tabs);
                }
                if let Some(p) = self.index.primary[top as usize] {
                    if self.visible(p) && self.enabled(p) {
                        add(LegendAction::Primary, LegendLabel::Primary(ItemIndex(p)));
                    }
                }
            }
        }
        self.legend = legend;
        self.legend_len = n;
    }
}
