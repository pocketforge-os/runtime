//! Retained, renderer-independent semantic scenes and focus traversal.
//!
//! This crate describes meaning and layout constraints. It deliberately contains no
//! pixels, colors, rendering backend, product routes, or fixed display dimensions.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

/// Stable identity for a semantic node across scene revisions.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NodeId(String);

impl NodeId {
    /// Creates an identifier. Empty identifiers are rejected.
    pub fn new(value: impl Into<String>) -> Result<Self, SceneError> {
        let value = value.into();
        if value.is_empty() {
            return Err(SceneError::EmptyNodeId);
        }
        Ok(Self(value))
    }

    /// Returns the identifier's string representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A node's accessibility role.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Group,
    Heading,
    Text,
    Button,
    Toggle,
    Slider,
    List,
    ListItem,
    Dialog,
}

/// A semantic action exposed by a focusable node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NodeAction {
    Activate,
    Back,
    SetValue(i32),
    Custom(String),
}

/// Structural state. Visual emphasis is derived from these bits by a theme.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NodeState {
    pub focused: bool,
    pub pressed: bool,
    pub disabled: bool,
    pub selected: bool,
    pub unavailable: bool,
    pub destructive: bool,
    /// Draw the theme scrim over this node's content (overlay-wave primitive).
    pub scrimmed: bool,
    pub checked: bool,
    pub expanded: bool,
}

/// Theme-owned depth treatment. Renderers resolve these to pre-baked assets.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Elevation {
    #[default]
    None,
    Elev1,
    Elev2,
    Focus,
}

/// Logical bounds constraints, independent of surface scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub min_width: f32,
    pub min_height: f32,
    pub max_width: Option<f32>,
    pub max_height: Option<f32>,
}

/// Content-addressed encoded image bytes supplied by the scene embedder.
///
/// The identifier must identify the bytes immutably. Renderers use it as their
/// decoded-image cache key and never resolve files or network resources.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageSource {
    pub id: String,
    pub bytes: Arc<[u8]>,
}

impl ImageSource {
    pub fn new(id: impl Into<String>, bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            id: id.into(),
            bytes: bytes.into(),
        }
    }
}

/// How an image is scaled while preserving its aspect ratio.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageFit {
    /// Fill the node bounds, cropping equally from the overflowing dimension.
    Cover,
    /// Show the entire image, centered inside the node bounds.
    Contain,
}

/// Renderer-independent visual content carried by a node.
#[derive(Clone, Debug, PartialEq)]
pub enum NodeContent {
    /// Render the accessible label as text (the legacy/default behavior).
    Label,
    /// Render encoded image bytes; the accessible label remains semantic alt text.
    Image { source: ImageSource, fit: ImageFit },
}

/// Product typography roles resolved by the renderer at presentation time.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum TypeRole {
    Hero,
    Title,
    H1,
    #[default]
    Body,
    Label,
    Caption,
    Eyebrow,
    /// Decorative edition-plate or monogram text; the only Fraunces binding.
    Plate,
}

impl Bounds {
    /// Creates unconstrained logical bounds at the supplied position and size.
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
            min_width: 0.0,
            min_height: 0.0,
            max_width: None,
            max_height: None,
        }
    }

    fn center(self) -> (f32, f32) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }
}

/// A semantic component in the retained tree.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub id: NodeId,
    pub role: Role,
    pub accessible_label: String,
    pub state: NodeState,
    pub bounds: Bounds,
    /// Theme-owned style key; never a resolved color or pixel value.
    pub style_token: String,
    /// `Some` makes the node focusable and states what activation means.
    pub action: Option<NodeAction>,
    pub content: NodeContent,
    /// Theme typography key. It is resolved to concrete metrics only while rendering.
    pub type_role: TypeRole,
    /// Component-owned line-height multiplier. `None` uses the font's normal line box.
    pub line_height: Option<f32>,
    /// Corner radius in logical pixels. Zero retains the sharp-rectangle geometry.
    pub corner_radius: f32,
    pub elevation: Elevation,
    pub children: Vec<Node>,
}

impl Node {
    pub fn new(
        id: NodeId,
        role: Role,
        accessible_label: impl Into<String>,
        bounds: Bounds,
        style_token: impl Into<String>,
    ) -> Self {
        Self {
            id,
            role,
            accessible_label: accessible_label.into(),
            state: NodeState::default(),
            bounds,
            style_token: style_token.into(),
            action: None,
            content: NodeContent::Label,
            type_role: TypeRole::Body,
            line_height: None,
            corner_radius: 0.0,
            elevation: Elevation::None,
            children: Vec::new(),
        }
    }

    pub fn with_action(mut self, action: NodeAction) -> Self {
        self.action = Some(action);
        self
    }

    pub fn with_children(mut self, children: Vec<Node>) -> Self {
        self.children = children;
        self
    }

    pub fn with_image(mut self, source: ImageSource, fit: ImageFit) -> Self {
        self.content = NodeContent::Image { source, fit };
        self
    }

    pub fn with_type_role(mut self, role: TypeRole) -> Self {
        self.type_role = role;
        self
    }

    /// Supplies component structure's line-height (for example 1.04 hero or 1.5 copy).
    pub fn with_line_height(mut self, multiplier: f32) -> Self {
        self.line_height = multiplier
            .is_finite()
            .then_some(multiplier)
            .filter(|v| *v > 0.0);
        self
    }

    pub fn with_elevation(mut self, elevation: Elevation) -> Self {
        self.elevation = elevation;
        self
    }

    /// Rounds this node's painted and clipped silhouette.
    ///
    /// Non-finite and non-positive values use the backward-compatible sharp shape;
    /// the renderer clamps positive values to half the shorter side.
    pub fn with_corner_radius(mut self, radius: f32) -> Self {
        self.corner_radius = if radius.is_finite() && radius > 0.0 {
            radius
        } else {
            0.0
        };
        self
    }

    pub fn is_focusable(&self) -> bool {
        self.action.is_some()
            && !self.state.disabled
            && !(self.state.unavailable && self.role.is_control())
    }
}

impl Role {
    fn is_control(self) -> bool {
        matches!(self, Self::Button | Self::Toggle | Self::Slider)
    }
}

/// Surface orientation supplied by the frame host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Orientation {
    Landscape,
    Portrait,
    LandscapeFlipped,
    PortraitFlipped,
}

/// Logical surface dimensions and insets. Consumers must not assume a global size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceMetrics {
    pub logical_width: f32,
    pub logical_height: f32,
    pub scale: f32,
    pub safe_insets: Insets,
    pub orientation: Orientation,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Insets {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AxisMove {
    Left,
    Right,
    Up,
    Down,
}

/// A controlled structural-state transition. Focus itself is owned by [`Scene`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateTransition {
    Pressed(bool),
    Disabled(bool),
    Selected(bool),
    Unavailable(bool),
    Destructive(bool),
    Scrimmed(bool),
    Checked(bool),
    Expanded(bool),
}

/// A validated scene with exactly one declared default-focus anchor.
#[derive(Clone, Debug, PartialEq)]
pub struct Scene {
    root: Node,
    default_focus: NodeId,
    focused: Option<NodeId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SceneError {
    EmptyNodeId,
    DuplicateNodeId(NodeId),
    DefaultFocusMissing(NodeId),
    DefaultFocusNotFocusable(NodeId),
    NodeMissing(NodeId),
    CannotDisableFocused(NodeId),
}

impl fmt::Display for SceneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for SceneError {}

impl Scene {
    /// Validates stable-ID uniqueness and establishes initial focus.
    ///
    /// Focus resolves to the declared anchor when it is focusable, then to the first
    /// focusable node in stable pre-order traversal. If no node is focusable, the scene
    /// has the explicit no-focus state reported by [`Scene::focused`] as `None`.
    pub fn new(mut root: Node, default_focus: NodeId) -> Result<Self, SceneError> {
        let mut ids = HashSet::new();
        validate_ids(&root, &mut ids)?;
        find(&root, &default_focus)
            .ok_or_else(|| SceneError::DefaultFocusMissing(default_focus.clone()))?;
        let focused = resolve_focus(&root, &default_focus);
        set_focus(&mut root, focused.as_ref());
        Ok(Self {
            root,
            focused,
            default_focus,
        })
    }

    pub fn root(&self) -> &Node {
        &self.root
    }

    pub fn default_focus(&self) -> &NodeId {
        &self.default_focus
    }

    /// Returns the focused node ID, or `None` when the scene has no focusable node.
    pub fn focused(&self) -> Option<&NodeId> {
        self.focused.as_ref()
    }

    /// Returns the focused node, or `None` when the scene has no focusable node.
    pub fn focused_node(&self) -> Option<&Node> {
        self.focused
            .as_ref()
            .map(|id| find(&self.root, id).expect("validated focus identifier"))
    }

    /// Moves toward the nearest focusable node in the requested half-plane.
    /// At an edge it returns `false` and leaves focus unchanged; traversal never wraps.
    pub fn move_focus(&mut self, direction: AxisMove) -> bool {
        let Some(focused_node) = self.focused_node() else {
            return false;
        };
        let current = focused_node.bounds.center();
        let mut candidates = Vec::new();
        collect_focusable(&self.root, &mut candidates);
        let next = candidates
            .into_iter()
            .filter(|node| Some(&node.id) != self.focused.as_ref())
            .filter_map(|node| {
                let center = node.bounds.center();
                directional_rank(current, center, direction).map(|rank| (rank, node.id.clone()))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, id)| id);
        if let Some(next) = next {
            set_focus(&mut self.root, Some(&next));
            self.focused = Some(next);
            true
        } else {
            false
        }
    }

    /// Restores focus using the scene's deterministic resolution order.
    ///
    /// Focus resolves to the declared anchor when it is focusable, then to the first
    /// focusable node in stable pre-order traversal. If no node is focusable, focus is
    /// cleared and [`Scene::focused`] reports `None`.
    pub fn reset_focus(&mut self) {
        let focused = resolve_focus(&self.root, &self.default_focus);
        set_focus(&mut self.root, focused.as_ref());
        self.focused = focused;
    }

    /// Applies semantic state without allowing callers to forge the focus bit.
    pub fn transition_state(
        &mut self,
        id: &NodeId,
        transition: StateTransition,
    ) -> Result<(), SceneError> {
        if self.focused.as_ref() == Some(id) && transition == StateTransition::Disabled(true) {
            return Err(SceneError::CannotDisableFocused(id.clone()));
        }
        {
            let node =
                find_mut(&mut self.root, id).ok_or_else(|| SceneError::NodeMissing(id.clone()))?;
            match transition {
                StateTransition::Pressed(value) => node.state.pressed = value,
                StateTransition::Disabled(value) => node.state.disabled = value,
                StateTransition::Selected(value) => node.state.selected = value,
                StateTransition::Unavailable(value) => node.state.unavailable = value,
                StateTransition::Destructive(value) => node.state.destructive = value,
                StateTransition::Scrimmed(value) => node.state.scrimmed = value,
                StateTransition::Checked(value) => node.state.checked = value,
                StateTransition::Expanded(value) => node.state.expanded = value,
            }
        }
        if self.focused.as_ref() == Some(id)
            && find(&self.root, id).is_some_and(|node| !node.is_focusable())
        {
            self.reset_focus();
        }
        Ok(())
    }
}

fn directional_rank(from: (f32, f32), to: (f32, f32), direction: AxisMove) -> Option<f32> {
    let (primary, orthogonal) = match direction {
        AxisMove::Left if to.0 < from.0 => (from.0 - to.0, (from.1 - to.1).abs()),
        AxisMove::Right if to.0 > from.0 => (to.0 - from.0, (from.1 - to.1).abs()),
        AxisMove::Up if to.1 < from.1 => (from.1 - to.1, (from.0 - to.0).abs()),
        AxisMove::Down if to.1 > from.1 => (to.1 - from.1, (from.0 - to.0).abs()),
        _ => return None,
    };
    Some(primary * primary + orthogonal * orthogonal * 4.0)
}

fn validate_ids(node: &Node, ids: &mut HashSet<NodeId>) -> Result<(), SceneError> {
    if !ids.insert(node.id.clone()) {
        return Err(SceneError::DuplicateNodeId(node.id.clone()));
    }
    for child in &node.children {
        validate_ids(child, ids)?;
    }
    Ok(())
}

fn find<'a>(node: &'a Node, id: &NodeId) -> Option<&'a Node> {
    if node.id == *id {
        return Some(node);
    }
    node.children.iter().find_map(|child| find(child, id))
}

fn find_mut<'a>(node: &'a mut Node, id: &NodeId) -> Option<&'a mut Node> {
    if node.id == *id {
        return Some(node);
    }
    node.children
        .iter_mut()
        .find_map(|child| find_mut(child, id))
}

fn resolve_focus(root: &Node, preferred: &NodeId) -> Option<NodeId> {
    find(root, preferred)
        .filter(|node| node.is_focusable())
        .or_else(|| first_focusable(root))
        .map(|node| node.id.clone())
}

fn first_focusable(node: &Node) -> Option<&Node> {
    node.is_focusable()
        .then_some(node)
        .or_else(|| node.children.iter().find_map(first_focusable))
}

fn set_focus(node: &mut Node, id: Option<&NodeId>) {
    node.state.focused = id == Some(&node.id);
    for child in &mut node.children {
        set_focus(child, id);
    }
}

fn collect_focusable<'a>(node: &'a Node, output: &mut Vec<&'a Node>) {
    if node.is_focusable() {
        output.push(node);
    }
    for child in &node.children {
        collect_focusable(child, output);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashSet, VecDeque};

    fn id(value: &str) -> NodeId {
        NodeId::new(value).unwrap()
    }

    fn button(name: String, x: f32, y: f32) -> Node {
        Node::new(
            id(&name),
            Role::Button,
            name,
            Bounds::new(x, y, 8.0, 8.0),
            "control",
        )
        .with_action(NodeAction::Activate)
    }

    fn grid(width: usize, height: usize) -> Scene {
        let nodes = (0..height)
            .flat_map(|y| {
                (0..width)
                    .map(move |x| button(format!("n-{x}-{y}"), x as f32 * 12.0, y as f32 * 12.0))
            })
            .collect();
        let root = Node::new(
            id("root"),
            Role::Group,
            "root",
            Bounds::new(0.0, 0.0, 1.0, 1.0),
            "root",
        )
        .with_children(nodes);
        Scene::new(root, id("n-0-0")).unwrap()
    }

    #[test]
    fn generated_layouts_are_reachable_by_axis_moves() {
        for width in 1..=7 {
            for height in 1..=7 {
                let initial = grid(width, height);
                let mut seen = HashSet::from([initial.focused().unwrap().clone()]);
                let mut queue = VecDeque::from([initial]);
                while let Some(scene) = queue.pop_front() {
                    for direction in [
                        AxisMove::Left,
                        AxisMove::Right,
                        AxisMove::Up,
                        AxisMove::Down,
                    ] {
                        let mut next = scene.clone();
                        next.move_focus(direction);
                        if seen.insert(next.focused().unwrap().clone()) {
                            queue.push_back(next);
                        }
                    }
                }
                assert_eq!(seen.len(), width * height, "{width}x{height} layout");
            }
        }
    }

    #[test]
    fn edges_do_not_wrap_and_focus_is_structural_state() {
        let mut scene = grid(3, 1);
        assert!(!scene.move_focus(AxisMove::Left));
        assert_eq!(scene.focused().unwrap().as_str(), "n-0-0");
        assert!(scene.focused_node().unwrap().state.focused);
        assert!(scene.move_focus(AxisMove::Right));
        assert!(scene.move_focus(AxisMove::Right));
        assert!(!scene.move_focus(AxisMove::Right));
        assert_eq!(scene.focused().unwrap().as_str(), "n-2-0");
    }

    #[test]
    fn validates_identifiers_and_default_anchor() {
        let duplicate = Node::new(
            id("root"),
            Role::Group,
            "root",
            Bounds::new(0.0, 0.0, 1.0, 1.0),
            "root",
        )
        .with_children(vec![
            button("same".into(), 0.0, 0.0),
            button("same".into(), 1.0, 0.0),
        ]);
        assert_eq!(
            Scene::new(duplicate, id("same")).unwrap_err(),
            SceneError::DuplicateNodeId(id("same"))
        );

        let non_focusable = Node::new(
            id("root"),
            Role::Group,
            "root",
            Bounds::new(0.0, 0.0, 1.0, 1.0),
            "root",
        );
        assert_eq!(
            Scene::new(non_focusable, id("root")).unwrap().focused(),
            None
        );
    }

    #[test]
    fn semantic_state_transitions_preserve_focus_ownership() {
        let mut scene = grid(2, 1);
        let second = id("n-1-0");
        for transition in [
            StateTransition::Selected(true),
            StateTransition::Checked(true),
            StateTransition::Expanded(true),
            StateTransition::Disabled(true),
        ] {
            scene.transition_state(&second, transition).unwrap();
        }
        assert!(!scene.move_focus(AxisMove::Right));
        assert_eq!(
            scene.transition_state(&id("n-0-0"), StateTransition::Disabled(true)),
            Err(SceneError::CannotDisableFocused(id("n-0-0")))
        );
        assert_eq!(
            scene.transition_state(&id("absent"), StateTransition::Selected(true)),
            Err(SceneError::NodeMissing(id("absent")))
        );
    }

    #[test]
    fn reset_focus_falls_back_when_the_declared_anchor_is_disabled() {
        let mut scene = grid(3, 1);
        assert!(scene.move_focus(AxisMove::Right));
        scene
            .transition_state(&id("n-0-0"), StateTransition::Disabled(true))
            .unwrap();

        scene.reset_focus();

        assert_eq!(scene.focused().unwrap().as_str(), "n-1-0");
        assert!(scene.focused_node().unwrap().is_focusable());
    }

    #[test]
    fn disabled_initial_anchor_falls_back_and_all_disabled_is_unfocused() {
        let mut disabled_anchor = button("anchor".into(), 0.0, 0.0);
        disabled_anchor.state.disabled = true;
        let fallback = button("fallback".into(), 12.0, 0.0);
        let root = Node::new(
            id("root"),
            Role::Group,
            "root",
            Bounds::new(0.0, 0.0, 1.0, 1.0),
            "root",
        )
        .with_children(vec![disabled_anchor.clone(), fallback]);
        let scene = Scene::new(root, id("anchor")).unwrap();
        assert_eq!(scene.focused().unwrap().as_str(), "fallback");

        let mut disabled_fallback = button("fallback".into(), 12.0, 0.0);
        disabled_fallback.state.disabled = true;
        let root = Node::new(
            id("root"),
            Role::Group,
            "root",
            Bounds::new(0.0, 0.0, 1.0, 1.0),
            "root",
        )
        .with_children(vec![disabled_anchor, disabled_fallback]);
        let scene = Scene::new(root, id("anchor")).unwrap();
        assert_eq!(scene.focused(), None);
        assert_eq!(scene.focused_node(), None);
        assert!(!scene.root().state.focused);
    }

    #[test]
    fn unavailable_item_card_stays_focusable_and_reachable() {
        let first = button("first".into(), 0.0, 0.0);
        let mut card = Node::new(
            id("card"),
            Role::ListItem,
            "Unavailable item details",
            Bounds::new(12.0, 0.0, 8.0, 8.0),
            "card",
        )
        .with_action(NodeAction::Activate);
        card.state.unavailable = true;
        assert!(
            card.is_focusable(),
            "unavailable item cards expose their details"
        );

        let root = Node::new(
            id("root"),
            Role::Group,
            "root",
            Bounds::new(0.0, 0.0, 1.0, 1.0),
            "root",
        )
        .with_children(vec![first, card]);
        let mut scene = Scene::new(root, id("first")).unwrap();
        assert!(scene.move_focus(AxisMove::Right));
        assert_eq!(scene.focused().unwrap().as_str(), "card");
    }

    #[test]
    fn unavailable_controls_are_skipped_for_initial_focus_and_traversal() {
        let first = button("first".into(), 0.0, 0.0);
        let mut unavailable = button("unavailable".into(), 12.0, 0.0);
        unavailable.state.unavailable = true;
        let last = button("last".into(), 24.0, 0.0);
        let root = Node::new(
            id("root"),
            Role::Group,
            "root",
            Bounds::new(0.0, 0.0, 1.0, 1.0),
            "root",
        )
        .with_children(vec![first, unavailable, last]);

        let initial = Scene::new(root.clone(), id("unavailable")).unwrap();
        assert_eq!(initial.focused().unwrap().as_str(), "first");

        let mut traversed = Scene::new(root, id("first")).unwrap();
        assert!(traversed.move_focus(AxisMove::Right));
        assert_eq!(traversed.focused().unwrap().as_str(), "last");
    }

    #[test]
    fn focused_control_refocuses_when_it_becomes_unavailable() {
        let mut scene = grid(2, 1);
        scene
            .transition_state(&id("n-0-0"), StateTransition::Unavailable(true))
            .unwrap();

        assert_eq!(scene.focused().unwrap().as_str(), "n-1-0");
        assert!(scene.focused_node().unwrap().state.focused);
        assert!(!find(scene.root(), &id("n-0-0")).unwrap().state.focused);
    }

    #[test]
    fn image_content_retains_encoded_bytes_and_fit() {
        let bytes: Arc<[u8]> = Arc::from(&b"encoded image"[..]);
        let node = Node::new(
            id("art"),
            Role::ListItem,
            "Accessible cover description",
            Bounds::new(1.0, 2.0, 30.0, 40.0),
            "cover",
        )
        .with_image(
            ImageSource::new("sha256:fixture", bytes.clone()),
            ImageFit::Cover,
        );
        assert_eq!(
            node.content,
            NodeContent::Image {
                source: ImageSource::new("sha256:fixture", bytes),
                fit: ImageFit::Cover,
            }
        );
    }

    #[test]
    fn corner_radius_defaults_sharp_and_sanitizes_builder_values() {
        let node = Node::new(
            id("card"),
            Role::Group,
            "card",
            Bounds::new(0.0, 0.0, 20.0, 10.0),
            "surface",
        );
        assert_eq!(node.corner_radius, 0.0);
        assert_eq!(node.clone().with_corner_radius(6.0).corner_radius, 6.0);
        assert_eq!(node.clone().with_corner_radius(f32::NAN).corner_radius, 0.0);
        assert_eq!(node.with_corner_radius(-1.0).corner_radius, 0.0);
    }
}
