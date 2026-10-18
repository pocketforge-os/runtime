//! Responsive semantic-scene lowering for [`pf_menu`].

use pf_menu::{ApplyState, Item, ItemKind, MenuEngine, MenuValue, Msg, Overlay};
use pf_scene::{
    AlignItems, Bounds, Edges, FlexDirection, JustifyContent, LayoutStyle, LayoutValue, Node,
    NodeAction, NodeId, Position, Role, Scene, SceneError, SurfaceMetrics, TypeRole,
};
use std::fmt;

/// Physical panel facts supplied by a device descriptor.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PanelFacts {
    pub diagonal_in: Option<f32>,
    pub active_mm: Option<(f32, f32)>,
}

impl PanelFacts {
    pub const fn diagonal(diagonal_in: f32) -> Self {
        Self {
            diagonal_in: Some(diagonal_in),
            active_mm: None,
        }
    }

    pub const fn active_mm(width: f32, height: f32) -> Self {
        Self {
            diagonal_in: None,
            active_mm: Some((width, height)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SizeClass {
    Compact,
    Regular,
    Large,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutMode {
    WideRail,
    TabStrip,
}

/// The five accessibility text-scale steps in the platform preference schema.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum TextScale {
    #[default]
    Percent100,
    Percent125,
    Percent150,
    Percent175,
    Percent200,
}

impl TextScale {
    pub const fn factor(self) -> f32 {
        match self {
            Self::Percent100 => 1.0,
            Self::Percent125 => 1.25,
            Self::Percent150 => 1.5,
            Self::Percent175 => 1.75,
            Self::Percent200 => 2.0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LowerError {
    MissingPanelFacts,
    InvalidPanelFacts,
    InvalidSurface,
    Scene(SceneError),
}

impl fmt::Display for LowerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for LowerError {}

impl From<SceneError> for LowerError {
    fn from(value: SceneError) -> Self {
        Self::Scene(value)
    }
}

/// Classifies physical panel size without falling back to pixel dimensions.
pub fn classify_size(facts: PanelFacts) -> Result<SizeClass, LowerError> {
    let diagonal = match (facts.diagonal_in, facts.active_mm) {
        (Some(value), _) => value,
        (None, Some((width, height))) => {
            if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
                return Err(LowerError::InvalidPanelFacts);
            }
            width.hypot(height) / 25.4
        }
        (None, None) => return Err(LowerError::MissingPanelFacts),
    };
    if !diagonal.is_finite() || diagonal <= 0.0 {
        return Err(LowerError::InvalidPanelFacts);
    }
    Ok(if diagonal < 4.5 {
        SizeClass::Compact
    } else if diagonal <= 5.25 {
        SizeClass::Regular
    } else {
        SizeClass::Large
    })
}

pub fn layout_mode(
    size_class: SizeClass,
    metrics: SurfaceMetrics,
    text_scale: TextScale,
) -> LayoutMode {
    let aspect = metrics.logical_width / metrics.logical_height;
    let four_by_three = aspect.is_finite() && (aspect - 4.0 / 3.0).abs() <= 0.02;
    if size_class == SizeClass::Compact || four_by_three || text_scale >= TextScale::Percent150 {
        LayoutMode::TabStrip
    } else {
        LayoutMode::WideRail
    }
}

/// Device-pixel values derived from the shared reference token table.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tokens {
    pub reference_scale: f32,
    pub spacing_1: f32,
    pub spacing_2: f32,
    pub spacing_3: f32,
    pub spacing_4: f32,
    pub safe_margin: f32,
    pub section_gap: f32,
    pub target: f32,
    pub row: f32,
    pub row_dense: f32,
    pub row_tall: f32,
    pub bar: f32,
    pub radius_row: f32,
    pub radius_modal: f32,
    pub focus_ring: f32,
}

impl Tokens {
    pub fn for_surface(size_class: SizeClass, metrics: SurfaceMetrics) -> Self {
        let aspect = metrics.logical_width / metrics.logical_height;
        let compact_four_by_three = size_class == SizeClass::Compact
            && aspect.is_finite()
            && (aspect - 4.0 / 3.0).abs() <= 0.02;
        let reference_scale = if compact_four_by_three {
            1.2
        } else {
            (metrics.logical_width / 1280.0).max(1.0)
        };
        let px = |reference: f32| (reference * reference_scale).round();
        Self {
            reference_scale,
            spacing_1: px(4.0),
            spacing_2: px(8.0),
            spacing_3: px(12.0),
            spacing_4: px(16.0),
            safe_margin: px(24.0),
            section_gap: px(32.0),
            target: px(48.0).max(48.0),
            row: px(56.0).max(48.0),
            row_dense: px(48.0).max(48.0),
            row_tall: px(72.0).max(48.0),
            bar: px(48.0).max(48.0),
            radius_row: px(12.0),
            radius_modal: px(16.0),
            focus_ring: px(4.0),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ThemeMode {
    Day,
    #[default]
    Dusk,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderPreferences {
    pub text_scale: TextScale,
    pub theme: ThemeMode,
    pub high_contrast: bool,
    pub reduce_motion: bool,
    /// Deterministically expands resolved strings by 30 percent for localization QA.
    pub pseudo_locale: bool,
}

impl Default for RenderPreferences {
    fn default() -> Self {
        Self {
            text_scale: TextScale::Percent100,
            theme: ThemeMode::Dusk,
            high_contrast: false,
            reduce_motion: false,
            pseudo_locale: false,
        }
    }
}

pub trait MsgResolver {
    fn resolve(&self, message: &Msg) -> String;
}

impl<F> MsgResolver for F
where
    F: Fn(&Msg) -> String,
{
    fn resolve(&self, message: &Msg) -> String {
        self(message)
    }
}

/// Resolver for already translated literals and deterministic message-id diagnostics.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResolvedMsg;

impl MsgResolver for ResolvedMsg {
    fn resolve(&self, message: &Msg) -> String {
        match message {
            Msg::Text(text) => text.text.clone(),
            Msg::Id(id) => id.id.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderMetadata {
    pub size_class: SizeClass,
    pub layout_mode: LayoutMode,
    pub tokens: Tokens,
    pub text_scale: TextScale,
    pub theme: ThemeMode,
    pub high_contrast: bool,
    pub transition_ms: u16,
    pub connections_folded: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoweredMenu {
    pub scene: Scene,
    pub metadata: RenderMetadata,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateKind {
    Rebuilt,
    FocusOnly,
    Unchanged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CacheKey {
    menu_id: String,
    revision: u64,
    page: pf_menu::PageIndex,
    overlay: Option<Overlay>,
    message_revision: u64,
    panel: [Option<u32>; 3],
    metrics: [u32; 8],
    text_scale: TextScale,
    theme: ThemeMode,
    high_contrast: bool,
    reduce_motion: bool,
    pseudo_locale: bool,
    focused_connection: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ItemSnapshot {
    value: Option<MenuValue>,
    apply: ApplyState,
    visible: bool,
    enabled: bool,
}

#[derive(Default)]
pub struct MenuSceneCache {
    key: Option<CacheKey>,
    items: Vec<ItemSnapshot>,
    lowered: Option<LoweredMenu>,
}

impl MenuSceneCache {
    pub fn lowered(&self) -> Option<&LoweredMenu> {
        self.lowered.as_ref()
    }

    /// Reuses retained scene structure for focus-only engine updates.
    ///
    /// `message_revision` must change whenever the resolver's output can change.
    pub fn update(
        &mut self,
        engine: &MenuEngine,
        messages: &dyn MsgResolver,
        message_revision: u64,
        panel: PanelFacts,
        metrics: SurfaceMetrics,
        preferences: RenderPreferences,
    ) -> Result<UpdateKind, LowerError> {
        let key = cache_key(engine, message_revision, panel, metrics, preferences);
        let items = item_snapshot(engine);
        if self.key.as_ref() == Some(&key) && self.items == items {
            let focused = engine
                .focus()
                .map(|index| node_id(&format!("menu/item/{}", engine.item(index).id.0)));
            let lowered = self.lowered.as_mut().expect("cache key implies a scene");
            if lowered.scene.focused() == focused.as_ref() {
                return Ok(UpdateKind::Unchanged);
            }
            lowered.scene.set_external_focus(focused)?;
            return Ok(UpdateKind::FocusOnly);
        }

        let lowered = lower(engine, messages, panel, metrics, preferences)?;
        self.key = Some(key);
        self.items = items;
        self.lowered = Some(lowered);
        Ok(UpdateKind::Rebuilt)
    }
}

fn cache_key(
    engine: &MenuEngine,
    message_revision: u64,
    panel: PanelFacts,
    metrics: SurfaceMetrics,
    preferences: RenderPreferences,
) -> CacheKey {
    CacheKey {
        menu_id: engine.menu().id.clone(),
        revision: engine.revision(),
        page: engine.page(),
        overlay: engine.overlay(),
        message_revision,
        panel: [
            panel.diagonal_in.map(f32::to_bits),
            panel.active_mm.map(|value| value.0.to_bits()),
            panel.active_mm.map(|value| value.1.to_bits()),
        ],
        metrics: [
            metrics.logical_width.to_bits(),
            metrics.logical_height.to_bits(),
            metrics.scale.to_bits(),
            metrics.safe_insets.top.to_bits(),
            metrics.safe_insets.right.to_bits(),
            metrics.safe_insets.bottom.to_bits(),
            metrics.safe_insets.left.to_bits(),
            metrics.orientation as u32,
        ],
        text_scale: preferences.text_scale,
        theme: preferences.theme,
        high_contrast: preferences.high_contrast,
        reduce_motion: preferences.reduce_motion,
        pseudo_locale: preferences.pseudo_locale,
        focused_connection: engine
            .focus()
            .is_some_and(|index| is_connection_item(engine.item(index))),
    }
}

fn item_snapshot(engine: &MenuEngine) -> Vec<ItemSnapshot> {
    (0..engine.item_count() as u32)
        .map(pf_menu::ItemIndex)
        .map(|index| ItemSnapshot {
            value: engine.value(index),
            apply: engine.apply_state(index),
            visible: engine.is_visible(index),
            enabled: engine.is_enabled(index),
        })
        .collect()
}

/// Lowers the active page of an engine into a renderer-independent semantic scene.
pub fn lower(
    engine: &MenuEngine,
    messages: &dyn MsgResolver,
    panel: PanelFacts,
    metrics: SurfaceMetrics,
    preferences: RenderPreferences,
) -> Result<LoweredMenu, LowerError> {
    validate_surface(metrics)?;
    let size_class = classify_size(panel)?;
    let mode = layout_mode(size_class, metrics, preferences.text_scale);
    let tokens = Tokens::for_surface(size_class, metrics);
    let page_index = engine.page();
    let page = engine.page_ref(page_index);
    let connections_folded = should_fold_connections(engine, metrics, mode, tokens);
    let connections_expanded = connections_folded
        && engine
            .focus()
            .is_some_and(|index| is_connection_item(engine.item(index)));
    let resolve = |message: &Msg| {
        let value = messages.resolve(message);
        if preferences.pseudo_locale {
            pseudo_expand(&value)
        } else {
            value
        }
    };

    let mut tabs = Vec::with_capacity(page.tabs.len());
    for tab_id in &page.tabs {
        let tab_page = engine
            .menu()
            .page(tab_id)
            .expect("validated menu tab target");
        let mut tab = Node::new(
            node_id(&format!("menu/tab/{}", tab_id.0)),
            Role::Tab,
            resolve(&tab_page.title),
            empty_bounds(),
            "--state-rest-surface",
        )
        .with_action(NodeAction::Custom(format!("tab:{}", tab_id.0)))
        .with_corner_radius(tokens.radius_row)
        .with_layout(tab_style(mode, tokens));
        tab.state.selected = tab_id == &page.id;
        tabs.push(tab);
    }

    let mut sections = Vec::with_capacity(page.sections.len());
    for (section_number, section) in page.sections.iter().enumerate() {
        let mut children = Vec::new();
        children.push(
            Node::new(
                node_id(&format!("menu/section/{section_number}/separator")),
                Role::Separator,
                "",
                empty_bounds(),
                "--color-border-hairline",
            )
            .with_layout(fixed_height(tokens.spacing_1)),
        );
        if let Some(header) = &section.header {
            children.push(
                text_node(
                    format!("menu/section/{section_number}/header"),
                    resolve(header),
                    TypeRole::Eyebrow,
                )
                .with_layout(fixed_height(
                    (24.0 * tokens.reference_scale)
                        .max(14.0 * 1.35 * preferences.text_scale.factor())
                        .round(),
                )),
            );
        }
        let mut connections_emitted = false;
        for item in &section.items {
            let index = engine
                .item_index(&item.id)
                .expect("validated menu item is indexed");
            if engine.page_of(index) == page_index && engine.is_visible(index) {
                if connections_folded && is_connection_item(item) {
                    if connections_emitted {
                        continue;
                    }
                    connections_emitted = true;
                    children.push(connections_node(
                        engine,
                        section,
                        messages,
                        preferences,
                        tokens,
                        connections_expanded,
                    ));
                } else {
                    children.push(item_node(
                        engine,
                        index,
                        item,
                        messages,
                        preferences,
                        tokens,
                    ));
                }
            }
        }
        if let Some(footer) = &section.footer {
            children.push(text_node(
                format!("menu/section/{section_number}/footer"),
                resolve(footer),
                TypeRole::Caption,
            ));
        }
        sections.push(
            Node::new(
                node_id(&format!("menu/section/{section_number}")),
                Role::Group,
                "",
                empty_bounds(),
                "--color-transparent",
            )
            .with_layout(column_style(tokens.spacing_2))
            .with_children(children),
        );
    }

    let title =
        text_node("menu/page/title", resolve(&page.title), TypeRole::H1).with_layout(fixed_height(
            tokens
                .bar
                .max(32.0 * 1.35 * preferences.text_scale.factor())
                .round(),
        ));
    let list = Node::new(
        node_id("menu/page/list"),
        Role::List,
        resolve(&page.title),
        empty_bounds(),
        "--color-transparent",
    )
    .with_layout(content_style(tokens))
    .with_children(std::iter::once(title).chain(sections).collect::<Vec<_>>());
    let navigation = Node::new(
        node_id("menu/navigation"),
        Role::List,
        "Pages",
        empty_bounds(),
        "--color-surface-sunken",
    )
    .with_layout(navigation_style(mode, tokens))
    .with_children(tabs);
    let body_children = match mode {
        LayoutMode::WideRail => vec![navigation, list],
        LayoutMode::TabStrip => vec![navigation, list],
    };
    let mut body = Node::new(
        node_id("menu/body"),
        Role::Group,
        "",
        empty_bounds(),
        "--color-transparent",
    )
    .with_layout(body_style(mode, tokens))
    .with_children(body_children);
    body.state.scrimmed = engine.overlay().is_some();
    let status = text_node("menu/status", resolve(&page.title), TypeRole::Caption)
        .with_layout(bar_style(tokens));
    let hint = Node::new(
        node_id("menu/hint"),
        Role::Group,
        "Controller prompts",
        empty_bounds(),
        "--color-surface-raised",
    )
    .with_layout(bar_style(tokens));
    let root_id = node_id(&format!("menu/root/{}", engine.menu().id));
    let mut root_children = vec![status, body, hint];
    let overlay = overlay_node(engine, messages, preferences, tokens);
    if let Some((overlay, _)) = &overlay {
        root_children.push(overlay.clone());
    }
    let root = Node::new(
        root_id.clone(),
        Role::Group,
        resolve(&page.title),
        Bounds::new(0.0, 0.0, metrics.logical_width, metrics.logical_height),
        "--color-surface-canvas",
    )
    .with_layout(root_style())
    .with_children(root_children);

    let focused = overlay
        .as_ref()
        .map(|(_, focused)| focused.clone())
        .or_else(|| {
            engine
                .focus()
                .map(|index| node_id(&format!("menu/item/{}", engine.item(index).id.0)))
        });
    let default_focus = focused.clone().unwrap_or(root_id);
    let scene = Scene::new_with_external_focus(root, default_focus, focused)?;
    Ok(LoweredMenu {
        scene,
        metadata: RenderMetadata {
            size_class,
            layout_mode: mode,
            tokens,
            text_scale: preferences.text_scale,
            theme: preferences.theme,
            high_contrast: preferences.high_contrast,
            transition_ms: if preferences.reduce_motion { 0 } else { 120 },
            connections_folded,
        },
    })
}

fn validate_surface(metrics: SurfaceMetrics) -> Result<(), LowerError> {
    if metrics.logical_width.is_finite()
        && metrics.logical_height.is_finite()
        && metrics.logical_width > 0.0
        && metrics.logical_height > 0.0
        && metrics.scale.is_finite()
        && metrics.scale > 0.0
    {
        Ok(())
    } else {
        Err(LowerError::InvalidSurface)
    }
}

fn item_node(
    engine: &MenuEngine,
    index: pf_menu::ItemIndex,
    item: &Item,
    messages: &dyn MsgResolver,
    preferences: RenderPreferences,
    tokens: Tokens,
) -> Node {
    let resolve = |message: &Msg| {
        let value = messages.resolve(message);
        if preferences.pseudo_locale {
            pseudo_expand(&value)
        } else {
            value
        }
    };
    let label = resolve(item.a11y.as_ref().unwrap_or(&item.label));
    let value = item_value(engine, index, item, &resolve);
    let accessible_label = value
        .as_ref()
        .map_or_else(|| label.clone(), |value| format!("{label}, {value}"));
    let role = item_role(&item.kind);
    let mut node = Node::new(
        node_id(&format!("menu/item/{}", item.id.0)),
        role,
        accessible_label,
        empty_bounds(),
        "--state-rest-surface",
    )
    .with_corner_radius(tokens.radius_row)
    .with_layout(row_style(item.subtitle.is_some(), tokens));
    if !matches!(item.kind, ItemKind::Info { .. } | ItemKind::Progress { .. }) {
        node = node.with_action(NodeAction::Activate);
    }
    node.state.disabled = !engine.is_enabled(index);
    node.state.unavailable = matches!(engine.apply_state(index), ApplyState::Reverted(_));
    node.state.pending = matches!(engine.apply_state(index), ApplyState::Pending { .. });
    node.state.checked = matches!(engine.value(index), Some(MenuValue::Bool(true)));
    node.state.destructive = matches!(
        &item.kind,
        ItemKind::Action {
            confirm: Some(confirm),
            ..
        } if confirm.destructive
    );
    node.state.value_text = value.clone();

    let mut main = vec![text_node(
        format!("menu/item/{}/label", item.id.0),
        resolve(&item.label),
        TypeRole::Body,
    )];
    if let Some(value) = value {
        main.push(text_node(
            format!("menu/item/{}/value", item.id.0),
            value,
            TypeRole::Label,
        ));
    }
    let mut content = vec![Node::new(
        node_id(&format!("menu/item/{}/main", item.id.0)),
        Role::Group,
        "",
        empty_bounds(),
        "--color-transparent",
    )
    .with_layout(row_line_style(
        tokens,
        preferences.text_scale >= TextScale::Percent150,
    ))
    .with_children(main)];
    if let Some(subtitle) = &item.subtitle {
        content.push(text_node(
            format!("menu/item/{}/subtitle", item.id.0),
            resolve(subtitle),
            TypeRole::Caption,
        ));
    }
    node.with_children(content)
}

fn connections_node(
    engine: &MenuEngine,
    section: &pf_menu::Section,
    messages: &dyn MsgResolver,
    preferences: RenderPreferences,
    tokens: Tokens,
    expanded: bool,
) -> Node {
    let children = if expanded {
        section
            .items
            .iter()
            .filter(|item| is_connection_item(item))
            .filter_map(|item| {
                let index = engine.item_index(&item.id)?;
                engine
                    .is_visible(index)
                    .then(|| item_node(engine, index, item, messages, preferences, tokens))
            })
            .collect()
    } else {
        vec![text_node(
            "menu/connections/label",
            "Connections".to_string(),
            TypeRole::Body,
        )]
    };
    let mut node = Node::new(
        node_id("menu/connections"),
        if expanded { Role::Group } else { Role::Link },
        "Connections",
        empty_bounds(),
        "--state-rest-surface",
    )
    .with_corner_radius(tokens.radius_row)
    .with_layout(if expanded {
        column_style(tokens.spacing_1)
    } else {
        row_style(false, tokens)
    })
    .with_children(children);
    node.state.expanded = expanded;
    if !expanded {
        node = node.with_action(NodeAction::Custom("connections".to_string()));
    }
    node
}

fn is_connection_item(item: &Item) -> bool {
    matches!(item.id.0.as_str(), "wifi" | "bt" | "airplane")
}

fn should_fold_connections(
    engine: &MenuEngine,
    metrics: SurfaceMetrics,
    mode: LayoutMode,
    tokens: Tokens,
) -> bool {
    let page = engine.page();
    let visible: Vec<_> = engine.visible_items(page).collect();
    if !["wifi", "bt", "airplane"].iter().all(|wanted| {
        visible
            .iter()
            .any(|index| engine.item(*index).id.0 == *wanted)
    }) {
        return false;
    }
    let rows = visible
        .iter()
        .map(|index| {
            if engine.item(*index).subtitle.is_some() {
                tokens.row_tall
            } else {
                tokens.row
            }
        })
        .sum::<f32>();
    let gaps = visible.len().saturating_sub(1) as f32 * tokens.spacing_2;
    let tabs = if mode == LayoutMode::TabStrip && !engine.page_ref(page).tabs.is_empty() {
        tokens.bar + tokens.spacing_2
    } else {
        0.0
    };
    let content_padding = 2.0 * (tokens.safe_margin - tokens.spacing_2).max(0.0);
    let content_band =
        metrics.logical_height - 2.0 * tokens.bar - tabs - tokens.bar - content_padding;
    rows + gaps > content_band.max(0.0)
}

fn item_role(kind: &ItemKind) -> Role {
    match kind {
        ItemKind::Action { .. } | ItemKind::Binding { .. } => Role::Button,
        ItemKind::Toggle { .. } => Role::Toggle,
        ItemKind::Choice { .. } | ItemKind::DateTime { .. } => Role::Choice,
        ItemKind::Stepper { .. } | ItemKind::Slider { .. } => Role::Slider,
        ItemKind::Info { .. } => Role::Text,
        ItemKind::Text { .. } => Role::TextField,
        ItemKind::Link { .. } => Role::Link,
        ItemKind::Progress { .. } => Role::Progress,
        ItemKind::Custom { .. } => Role::Group,
    }
}

fn item_value(
    engine: &MenuEngine,
    index: pf_menu::ItemIndex,
    item: &Item,
    resolve: &dyn Fn(&Msg) -> String,
) -> Option<String> {
    match (&item.kind, engine.value(index)) {
        (ItemKind::Toggle { .. }, Some(MenuValue::Bool(value))) => {
            Some(if value { "On" } else { "Off" }.to_string())
        }
        (ItemKind::Choice { options, .. }, Some(MenuValue::Choice(value))) => options
            .get(value as usize)
            .map(|option| resolve(&option.label)),
        (ItemKind::Stepper { unit, .. }, Some(MenuValue::Int(value))) => Some(match unit {
            Some(unit) => format!("{value} {}", resolve(unit)),
            None => value.to_string(),
        }),
        (ItemKind::Slider { .. } | ItemKind::Progress { .. }, Some(MenuValue::Int(value))) => {
            Some(value.to_string())
        }
        (
            ItemKind::Info {
                value: Some(value), ..
            },
            _,
        ) => Some(resolve(value)),
        (ItemKind::Binding { action, .. }, _) => Some(action.clone()),
        (ItemKind::DateTime { .. }, Some(MenuValue::Int(value))) => Some(value.to_string()),
        _ => None,
    }
}

fn overlay_node(
    engine: &MenuEngine,
    messages: &dyn MsgResolver,
    preferences: RenderPreferences,
    tokens: Tokens,
) -> Option<(Node, NodeId)> {
    let resolve = |message: &Msg| {
        let value = messages.resolve(message);
        if preferences.pseudo_locale {
            pseudo_expand(&value)
        } else {
            value
        }
    };
    let (content, focused) = match engine.overlay()? {
        Overlay::Confirm {
            item,
            confirm_focused,
        } => {
            let ItemKind::Action {
                confirm: Some(confirm),
                ..
            } = &engine.item(item).kind
            else {
                unreachable!("validated confirm overlay belongs to a confirm action")
            };
            let cancel_id = node_id("menu/overlay/cancel");
            let confirm_id = node_id("menu/overlay/confirm");
            let mut confirm_button = Node::new(
                confirm_id.clone(),
                Role::Button,
                resolve(&confirm.confirm_label),
                empty_bounds(),
                "--state-rest-surface",
            )
            .with_action(NodeAction::Activate)
            .with_corner_radius(tokens.radius_row)
            .with_layout(overlay_button_style(tokens))
            .with_children(vec![text_node(
                "menu/overlay/confirm/label",
                resolve(&confirm.confirm_label),
                TypeRole::Label,
            )]);
            confirm_button.state.destructive = confirm.destructive;
            let cancel_button = Node::new(
                cancel_id.clone(),
                Role::Button,
                "Cancel",
                empty_bounds(),
                "--state-rest-surface",
            )
            .with_action(NodeAction::Back)
            .with_corner_radius(tokens.radius_row)
            .with_layout(overlay_button_style(tokens))
            .with_children(vec![text_node(
                "menu/overlay/cancel/label",
                "Cancel".to_string(),
                TypeRole::Label,
            )]);
            let mut children = vec![text_node(
                "menu/overlay/title",
                resolve(&confirm.title),
                TypeRole::Title,
            )];
            if let Some(body) = &confirm.body {
                children.push(text_node(
                    "menu/overlay/body",
                    resolve(body),
                    TypeRole::Body,
                ));
            }
            children.push(
                Node::new(
                    node_id("menu/overlay/actions"),
                    Role::Group,
                    "",
                    empty_bounds(),
                    "--color-transparent",
                )
                .with_layout(overlay_actions_style(tokens))
                .with_children(vec![cancel_button, confirm_button]),
            );
            (
                Node::new(
                    node_id("menu/overlay/card"),
                    Role::Group,
                    resolve(&confirm.title),
                    empty_bounds(),
                    "--color-surface-overlay",
                )
                .with_corner_radius(tokens.radius_modal)
                .with_layout(overlay_card_style(tokens))
                .with_children(children),
                if confirm_focused {
                    confirm_id
                } else {
                    cancel_id
                },
            )
        }
        Overlay::Picker { item, cursor } => {
            let ItemKind::Choice { options, .. } = &engine.item(item).kind else {
                unreachable!("validated picker overlay belongs to a choice")
            };
            let focused = node_id(&format!("menu/overlay/option/{cursor}"));
            let options = options
                .iter()
                .enumerate()
                .map(|(index, option)| {
                    let label = resolve(&option.label);
                    let mut children = vec![text_node(
                        format!("menu/overlay/option/{index}/label"),
                        label.clone(),
                        TypeRole::Body,
                    )];
                    if let Some(detail) = &option.detail {
                        children.push(text_node(
                            format!("menu/overlay/option/{index}/detail"),
                            resolve(detail),
                            TypeRole::Caption,
                        ));
                    }
                    let mut node = Node::new(
                        node_id(&format!("menu/overlay/option/{index}")),
                        Role::Choice,
                        label,
                        empty_bounds(),
                        "--state-rest-surface",
                    )
                    .with_action(NodeAction::Activate)
                    .with_corner_radius(tokens.radius_row)
                    .with_layout(row_style(option.detail.is_some(), tokens))
                    .with_children(children);
                    node.state.selected = index == cursor as usize;
                    node
                })
                .collect();
            (
                Node::new(
                    node_id("menu/overlay/card"),
                    Role::List,
                    resolve(&engine.item(item).label),
                    empty_bounds(),
                    "--color-surface-overlay",
                )
                .with_corner_radius(tokens.radius_modal)
                .with_layout(overlay_card_style(tokens))
                .with_children(options),
                focused,
            )
        }
    };
    Some((
        Node::new(
            node_id("menu/overlay"),
            Role::Dialog,
            content.accessible_label.clone(),
            empty_bounds(),
            "--color-transparent",
        )
        .with_layout(overlay_style())
        .with_children(vec![content]),
        focused,
    ))
}

fn pseudo_expand(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let extra = ((value.chars().count() as f32) * 0.3).ceil() as usize;
    format!("{value}{}", "~".repeat(extra))
}

fn node_id(value: &str) -> NodeId {
    NodeId::new(value).expect("menu scene ids have a non-empty static prefix")
}

fn empty_bounds() -> Bounds {
    Bounds::new(0.0, 0.0, 0.0, 0.0)
}

fn text_node(id: impl AsRef<str>, label: String, type_role: TypeRole) -> Node {
    Node::new(
        node_id(id.as_ref()),
        Role::Text,
        label,
        empty_bounds(),
        "--color-transparent",
    )
    .with_type_role(type_role)
    .with_layout(LayoutStyle::default())
}

fn edges(value: f32) -> Edges<LayoutValue> {
    Edges {
        top: LayoutValue::Px(value),
        right: LayoutValue::Px(value),
        bottom: LayoutValue::Px(value),
        left: LayoutValue::Px(value),
    }
}

fn root_style() -> LayoutStyle {
    LayoutStyle {
        width: LayoutValue::Pct(1.0),
        height: LayoutValue::Pct(1.0),
        flex_direction: FlexDirection::Column,
        ..LayoutStyle::default()
    }
}

fn bar_style(tokens: Tokens) -> LayoutStyle {
    LayoutStyle {
        width: LayoutValue::Pct(1.0),
        height: LayoutValue::Px(tokens.bar),
        min_height: LayoutValue::Px(tokens.bar),
        padding: Edges {
            left: LayoutValue::Px(tokens.safe_margin),
            right: LayoutValue::Px(tokens.safe_margin),
            ..edges(0.0)
        },
        align_items: Some(AlignItems::Center),
        ..LayoutStyle::default()
    }
}

fn body_style(mode: LayoutMode, tokens: Tokens) -> LayoutStyle {
    LayoutStyle {
        width: LayoutValue::Pct(1.0),
        flex_grow: 1.0,
        flex_shrink: 1.0,
        flex_direction: match mode {
            LayoutMode::WideRail => FlexDirection::Row,
            LayoutMode::TabStrip => FlexDirection::Column,
        },
        gap: (
            LayoutValue::Px(tokens.spacing_2),
            LayoutValue::Px(tokens.spacing_2),
        ),
        padding: Edges {
            left: LayoutValue::Px(tokens.spacing_2),
            right: LayoutValue::Px(tokens.spacing_2),
            ..edges(0.0)
        },
        ..LayoutStyle::default()
    }
}

fn navigation_style(mode: LayoutMode, tokens: Tokens) -> LayoutStyle {
    LayoutStyle {
        flex_direction: match mode {
            LayoutMode::WideRail => FlexDirection::Column,
            LayoutMode::TabStrip => FlexDirection::Row,
        },
        width: match mode {
            LayoutMode::WideRail => LayoutValue::Px((336.0 * tokens.reference_scale).round()),
            LayoutMode::TabStrip => LayoutValue::Pct(1.0),
        },
        height: match mode {
            LayoutMode::WideRail => LayoutValue::Pct(1.0),
            LayoutMode::TabStrip => LayoutValue::Px(tokens.bar),
        },
        gap: (
            LayoutValue::Px(tokens.spacing_2),
            LayoutValue::Px(tokens.spacing_2),
        ),
        ..LayoutStyle::default()
    }
}

fn tab_style(mode: LayoutMode, tokens: Tokens) -> LayoutStyle {
    LayoutStyle {
        min_height: LayoutValue::Px(tokens.target),
        flex_grow: u8::from(mode == LayoutMode::TabStrip) as f32,
        padding: Edges {
            left: LayoutValue::Px(tokens.spacing_4),
            right: LayoutValue::Px(tokens.spacing_4),
            ..edges(tokens.spacing_2)
        },
        align_items: Some(AlignItems::Center),
        ..LayoutStyle::default()
    }
}

fn content_style(tokens: Tokens) -> LayoutStyle {
    LayoutStyle {
        flex_direction: FlexDirection::Column,
        flex_grow: 1.0,
        flex_shrink: 1.0,
        padding: edges(tokens.safe_margin - tokens.spacing_2),
        gap: (
            LayoutValue::Px(tokens.spacing_2),
            LayoutValue::Px(tokens.spacing_2),
        ),
        ..LayoutStyle::default()
    }
}

fn column_style(gap: f32) -> LayoutStyle {
    LayoutStyle {
        width: LayoutValue::Pct(1.0),
        flex_direction: FlexDirection::Column,
        gap: (LayoutValue::Px(gap), LayoutValue::Px(gap)),
        ..LayoutStyle::default()
    }
}

fn row_style(two_line: bool, tokens: Tokens) -> LayoutStyle {
    LayoutStyle {
        width: LayoutValue::Pct(1.0),
        min_height: LayoutValue::Px(if two_line {
            tokens.row_tall
        } else {
            tokens.row
        }),
        flex_direction: FlexDirection::Column,
        align_items: Some(AlignItems::Stretch),
        justify_content: Some(JustifyContent::Center),
        gap: (
            LayoutValue::Px(tokens.spacing_1),
            LayoutValue::Px(tokens.spacing_1),
        ),
        padding: Edges {
            top: LayoutValue::Px(tokens.spacing_2),
            right: LayoutValue::Px(tokens.spacing_4),
            bottom: LayoutValue::Px(tokens.spacing_2),
            left: LayoutValue::Px(tokens.spacing_4),
        },
        ..LayoutStyle::default()
    }
}

fn row_line_style(tokens: Tokens, stacked: bool) -> LayoutStyle {
    LayoutStyle {
        width: LayoutValue::Pct(1.0),
        flex_direction: if stacked {
            FlexDirection::Column
        } else {
            FlexDirection::Row
        },
        align_items: Some(if stacked {
            AlignItems::Stretch
        } else {
            AlignItems::Center
        }),
        justify_content: Some(if stacked {
            JustifyContent::Start
        } else {
            JustifyContent::SpaceBetween
        }),
        gap: (
            LayoutValue::Px(tokens.spacing_4),
            LayoutValue::Px(tokens.spacing_2),
        ),
        ..LayoutStyle::default()
    }
}

fn fixed_height(height: f32) -> LayoutStyle {
    LayoutStyle {
        width: LayoutValue::Pct(1.0),
        height: LayoutValue::Px(height),
        min_height: LayoutValue::Px(height),
        ..LayoutStyle::default()
    }
}

fn overlay_style() -> LayoutStyle {
    LayoutStyle {
        position: Position::Absolute,
        width: LayoutValue::Pct(1.0),
        height: LayoutValue::Pct(1.0),
        inset: edges(0.0),
        align_items: Some(AlignItems::Center),
        justify_content: Some(JustifyContent::Center),
        ..LayoutStyle::default()
    }
}

fn overlay_card_style(tokens: Tokens) -> LayoutStyle {
    LayoutStyle {
        width: LayoutValue::Pct(0.46),
        min_width: LayoutValue::Px(320.0 * tokens.reference_scale),
        flex_direction: FlexDirection::Column,
        gap: (
            LayoutValue::Px(tokens.spacing_3),
            LayoutValue::Px(tokens.spacing_3),
        ),
        padding: edges(tokens.safe_margin),
        ..LayoutStyle::default()
    }
}

fn overlay_actions_style(tokens: Tokens) -> LayoutStyle {
    LayoutStyle {
        width: LayoutValue::Pct(1.0),
        justify_content: Some(JustifyContent::End),
        gap: (
            LayoutValue::Px(tokens.spacing_2),
            LayoutValue::Px(tokens.spacing_2),
        ),
        ..LayoutStyle::default()
    }
}

fn overlay_button_style(tokens: Tokens) -> LayoutStyle {
    LayoutStyle {
        min_height: LayoutValue::Px(tokens.target),
        padding: Edges {
            left: LayoutValue::Px(tokens.safe_margin),
            right: LayoutValue::Px(tokens.safe_margin),
            ..edges(0.0)
        },
        align_items: Some(AlignItems::Center),
        ..LayoutStyle::default()
    }
}
