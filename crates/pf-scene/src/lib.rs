//! Retained, renderer-independent semantic scenes and focus traversal.
//!
//! This crate describes meaning and layout constraints. It deliberately contains no
//! pixels, colors, rendering backend, product routes, or fixed display dimensions.

use std::collections::HashSet;
use std::fmt;

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
    pub disabled: bool,
    pub selected: bool,
    pub checked: bool,
    pub expanded: bool,
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

    pub fn is_focusable(&self) -> bool {
        self.action.is_some() && !self.state.disabled
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
    Disabled(bool),
    Selected(bool),
    Checked(bool),
    Expanded(bool),
}

/// A validated scene with exactly one declared default-focus anchor.
#[derive(Clone, Debug, PartialEq)]
pub struct Scene {
    root: Node,
    default_focus: NodeId,
    focused: NodeId,
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
    /// Validates stable-ID uniqueness and focuses the declared default anchor.
    pub fn new(mut root: Node, default_focus: NodeId) -> Result<Self, SceneError> {
        let mut ids = HashSet::new();
        validate_ids(&root, &mut ids)?;
        let anchor = find(&root, &default_focus)
            .ok_or_else(|| SceneError::DefaultFocusMissing(default_focus.clone()))?;
        if !anchor.is_focusable() {
            return Err(SceneError::DefaultFocusNotFocusable(default_focus));
        }
        set_focus(&mut root, &default_focus);
        Ok(Self {
            root,
            focused: default_focus.clone(),
            default_focus,
        })
    }

    pub fn root(&self) -> &Node {
        &self.root
    }

    pub fn default_focus(&self) -> &NodeId {
        &self.default_focus
    }

    pub fn focused(&self) -> &NodeId {
        &self.focused
    }

    pub fn focused_node(&self) -> &Node {
        find(&self.root, &self.focused).expect("validated focus identifier")
    }

    /// Moves toward the nearest focusable node in the requested half-plane.
    /// At an edge it returns `false` and leaves focus unchanged; traversal never wraps.
    pub fn move_focus(&mut self, direction: AxisMove) -> bool {
        let current = self.focused_node().bounds.center();
        let mut candidates = Vec::new();
        collect_focusable(&self.root, &mut candidates);
        let next = candidates
            .into_iter()
            .filter(|node| node.id != self.focused)
            .filter_map(|node| {
                let center = node.bounds.center();
                directional_rank(current, center, direction).map(|rank| (rank, node.id.clone()))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, id)| id);
        if let Some(next) = next {
            set_focus(&mut self.root, &next);
            self.focused = next;
            true
        } else {
            false
        }
    }

    /// Restores the declared default anchor.
    pub fn reset_focus(&mut self) {
        set_focus(&mut self.root, &self.default_focus);
        self.focused = self.default_focus.clone();
    }

    /// Applies semantic state without allowing callers to forge the focus bit.
    pub fn transition_state(
        &mut self,
        id: &NodeId,
        transition: StateTransition,
    ) -> Result<(), SceneError> {
        if id == &self.focused && transition == StateTransition::Disabled(true) {
            return Err(SceneError::CannotDisableFocused(id.clone()));
        }
        let node =
            find_mut(&mut self.root, id).ok_or_else(|| SceneError::NodeMissing(id.clone()))?;
        match transition {
            StateTransition::Disabled(value) => node.state.disabled = value,
            StateTransition::Selected(value) => node.state.selected = value,
            StateTransition::Checked(value) => node.state.checked = value,
            StateTransition::Expanded(value) => node.state.expanded = value,
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

fn set_focus(node: &mut Node, id: &NodeId) {
    node.state.focused = node.id == *id;
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
                let mut seen = HashSet::from([initial.focused().clone()]);
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
                        if seen.insert(next.focused().clone()) {
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
        assert_eq!(scene.focused().as_str(), "n-0-0");
        assert!(scene.focused_node().state.focused);
        assert!(scene.move_focus(AxisMove::Right));
        assert!(scene.move_focus(AxisMove::Right));
        assert!(!scene.move_focus(AxisMove::Right));
        assert_eq!(scene.focused().as_str(), "n-2-0");
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
            Scene::new(non_focusable, id("root")).unwrap_err(),
            SceneError::DefaultFocusNotFocusable(id("root"))
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
}
