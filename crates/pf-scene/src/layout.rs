use crate::{Bounds, Insets, Node, NodeContent, NodeId, Role, SurfaceMetrics, TextAlign, TypeRole};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use taffy::prelude as tf;
use taffy::{geometry::Point, style::Overflow as TaffyOverflow, TaffyError};

pub type Metrics = SurfaceMetrics;
type MeasureContext = (String, TypeRole, TextAlign, Option<f32>);
type LayoutTree = tf::TaffyTree<Option<MeasureContext>>;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum LayoutValue {
    Px(f32),
    Pct(f32),
    #[default]
    Auto,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Track {
    Px(f32),
    Fr(f32),
    Auto,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Display {
    #[default]
    Flex,
    Grid,
    None,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FlexDirection {
    #[default]
    Row,
    Column,
    RowReverse,
    ColumnReverse,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AlignItems {
    Start,
    End,
    Center,
    Stretch,
    Baseline,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JustifyContent {
    Start,
    End,
    Center,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Overflow {
    #[default]
    Visible,
    Hidden,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BoxSizing {
    #[default]
    BorderBox,
    ContentBox,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Position {
    #[default]
    Relative,
    Absolute,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Edges<T: Copy + Default> {
    pub top: T,
    pub right: T,
    pub bottom: T,
    pub left: T,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LayoutStyle {
    pub display: Display,
    pub position: Position,
    pub flex_direction: FlexDirection,
    pub align_items: Option<AlignItems>,
    pub justify_content: Option<JustifyContent>,
    pub flex_grow: f32,
    pub flex_shrink: f32,
    pub flex_basis: LayoutValue,
    pub gap: (LayoutValue, LayoutValue),
    pub padding: Edges<LayoutValue>,
    pub margin: Edges<LayoutValue>,
    pub width: LayoutValue,
    pub height: LayoutValue,
    pub min_width: LayoutValue,
    pub min_height: LayoutValue,
    pub max_width: LayoutValue,
    pub max_height: LayoutValue,
    pub inset: Edges<LayoutValue>,
    pub overflow: (Overflow, Overflow),
    pub box_sizing: BoxSizing,
    pub grid_template_columns: Vec<Track>,
}

impl Default for LayoutStyle {
    fn default() -> Self {
        Self {
            display: Display::Flex,
            position: Position::Relative,
            flex_direction: FlexDirection::Row,
            align_items: None,
            justify_content: None,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: LayoutValue::Auto,
            gap: (LayoutValue::Px(0.0), LayoutValue::Px(0.0)),
            padding: Edges::default(),
            margin: Edges::default(),
            width: LayoutValue::Auto,
            height: LayoutValue::Auto,
            min_width: LayoutValue::Auto,
            min_height: LayoutValue::Auto,
            max_width: LayoutValue::Auto,
            max_height: LayoutValue::Auto,
            inset: Edges::default(),
            overflow: (Overflow::Visible, Overflow::Visible),
            box_sizing: BoxSizing::BorderBox,
            grid_template_columns: vec![],
        }
    }
}

pub trait TextMeasure {
    fn measure(
        &self,
        text: &str,
        role: TypeRole,
        align: TextAlign,
        scale: f32,
        max_width: Option<f32>,
        line_height: Option<f32>,
    ) -> (f32, f32);
}

#[derive(Default)]
pub struct LayoutCache {
    entries: HashMap<CacheKey, Vec<(NodeId, Bounds)>>,
    typography_revision: u64,
    hits: u64,
}
impl LayoutCache {
    pub fn invalidate_all(&mut self) {
        self.entries.clear();
    }
    pub fn set_typography_revision(&mut self, revision: u64) {
        if self.typography_revision != revision {
            self.typography_revision = revision;
            self.invalidate_all();
        }
    }
    pub fn hits(&self) -> u64 {
        self.hits
    }
}
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CacheKey {
    root: NodeId,
    metrics: [u32; 8],
    text_scale: u32,
    typography_revision: u64,
    inputs: u64,
}

pub fn resolve_layout(
    root: &mut Node,
    metrics: Metrics,
    text_scale: f32,
    measure: &dyn TextMeasure,
    cache: &mut LayoutCache,
) {
    resolve_walk(root, metrics, None, text_scale, measure, cache);
}

fn resolve_walk(
    node: &mut Node,
    metrics: Metrics,
    enclosing_legacy: Option<Bounds>,
    scale: f32,
    measure: &dyn TextMeasure,
    cache: &mut LayoutCache,
) {
    if node.layout.is_some() {
        let (metrics, origin) = enclosing_legacy.map_or((metrics, None), |bounds| {
            (
                Metrics {
                    logical_width: bounds.width,
                    logical_height: bounds.height,
                    scale: metrics.scale,
                    safe_insets: Insets::default(),
                    orientation: metrics.orientation,
                },
                Some((bounds.x, bounds.y)),
            )
        });
        resolve_subtree(node, metrics, origin, scale, measure, cache);
        resolve_legacy_islands(node, metrics, scale, measure, cache);
        return;
    }
    for child in &mut node.children {
        resolve_walk(child, metrics, Some(node.bounds), scale, measure, cache);
    }
}

fn resolve_legacy_islands(
    node: &mut Node,
    metrics: Metrics,
    scale: f32,
    measure: &dyn TextMeasure,
    cache: &mut LayoutCache,
) {
    for child in &mut node.children {
        if child.layout.is_none() {
            // Taffy treated this island as an opaque leaf. Its own box is resolved, but
            // its descendants still need the normal walk, enclosed by that box.
            for descendant in &mut child.children {
                resolve_walk(
                    descendant,
                    metrics,
                    Some(child.bounds),
                    scale,
                    measure,
                    cache,
                );
            }
        } else {
            // This styled child was already resolved as part of `node`'s subtree. Scan
            // through it for opaque islands without resolving it again as a fresh root.
            resolve_legacy_islands(child, metrics, scale, measure, cache);
        }
    }
}

fn resolve_subtree(
    root: &mut Node,
    metrics: Metrics,
    containing_origin: Option<(f32, f32)>,
    scale: f32,
    measure: &dyn TextMeasure,
    cache: &mut LayoutCache,
) {
    let key = CacheKey {
        root: root.id.clone(),
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
        text_scale: scale.to_bits(),
        typography_revision: cache.typography_revision,
        inputs: input_hash(root),
    };
    let root_origin = containing_origin.unwrap_or((root.bounds.x, root.bounds.y));
    if let Some(bounds) = cache.entries.get(&key).cloned() {
        cache.hits += 1;
        apply_bounds(root, &bounds, root_origin);
        return;
    }
    let mut tree = LayoutTree::new();
    // Bounds are logical values. Pixel snapping belongs to the renderer and would break
    // exact explicit-bounds legacy islands here.
    tree.disable_rounding();
    let mut ids = HashMap::new();
    let root_id =
        build_node(&mut tree, root, measure, scale, true, &mut ids).expect("valid Taffy tree");
    tree.compute_layout_with_measure(
        root_id,
        tf::Size {
            width: tf::AvailableSpace::Definite(
                (metrics.logical_width - metrics.safe_insets.left - metrics.safe_insets.right)
                    .max(0.0),
            ),
            height: tf::AvailableSpace::Definite(
                (metrics.logical_height - metrics.safe_insets.top - metrics.safe_insets.bottom)
                    .max(0.0),
            ),
        },
        |known, available, _, context, _| {
            let Some(Some((text, role, align, line_height))) = context else {
                return tf::Size::ZERO;
            };
            let max_width = known.width.or(match available.width {
                tf::AvailableSpace::Definite(v) => Some(v),
                _ => None,
            });
            let (width, height) =
                measure.measure(text, *role, *align, scale, max_width, *line_height);
            tf::Size {
                width: known.width.unwrap_or(width),
                height: known.height.unwrap_or(height),
            }
        },
    )
    .expect("layout computation succeeds");
    write_root_layout(&tree, root, &ids, root_origin, metrics.safe_insets);
    let mut result = Vec::new();
    collect_bounds(root, root_origin, &mut result);
    cache.entries.insert(key, result);
}

fn build_node(
    tree: &mut LayoutTree,
    node: &Node,
    measure: &dyn TextMeasure,
    scale: f32,
    participating: bool,
    ids: &mut HashMap<NodeId, tf::NodeId>,
) -> Result<tf::NodeId, TaffyError> {
    let legacy = participating && node.layout.is_none();
    let style = if legacy {
        legacy_style(node.bounds)
    } else {
        to_taffy(node.layout.as_ref().expect("participating node has style"))
    };
    let children = if legacy {
        vec![]
    } else {
        node.children
            .iter()
            .map(|child| build_node(tree, child, measure, scale, true, ids))
            .collect::<Result<Vec<_>, _>>()?
    };
    let label = if matches!(node.content, NodeContent::Label)
        && matches!(node.role, Role::Text | Role::Heading)
    {
        Some((
            node.accessible_label.clone(),
            node.type_role,
            node.text_align,
            node.line_height,
        ))
    } else {
        None
    };
    let id = tree.new_leaf_with_context(style, label).and_then(|id| {
        if children.is_empty() {
            Ok(id)
        } else {
            tree.set_children(id, &children)?;
            Ok(id)
        }
    })?;
    ids.insert(node.id.clone(), id);
    let _ = (measure, scale);
    Ok(id)
}

fn to_taffy(s: &LayoutStyle) -> tf::Style {
    tf::Style {
        display: match s.display {
            Display::Flex => tf::Display::Flex,
            Display::Grid => tf::Display::Grid,
            Display::None => tf::Display::None,
        },
        position: match s.position {
            Position::Relative => tf::Position::Relative,
            Position::Absolute => tf::Position::Absolute,
        },
        flex_direction: match s.flex_direction {
            FlexDirection::Row => tf::FlexDirection::Row,
            FlexDirection::Column => tf::FlexDirection::Column,
            FlexDirection::RowReverse => tf::FlexDirection::RowReverse,
            FlexDirection::ColumnReverse => tf::FlexDirection::ColumnReverse,
        },
        align_items: s.align_items.map(map_align),
        justify_content: s.justify_content.map(map_justify),
        flex_grow: s.flex_grow,
        flex_shrink: s.flex_shrink,
        flex_basis: dim(s.flex_basis),
        gap: tf::Size {
            width: length(s.gap.1),
            height: length(s.gap.0),
        },
        padding: rect_len(s.padding),
        margin: rect_auto(s.margin),
        size: tf::Size {
            width: dim(s.width),
            height: dim(s.height),
        },
        min_size: tf::Size {
            width: dim(s.min_width),
            height: dim(s.min_height),
        },
        max_size: tf::Size {
            width: dim(s.max_width),
            height: dim(s.max_height),
        },
        inset: rect_auto(s.inset),
        overflow: Point {
            x: map_overflow(s.overflow.0),
            y: map_overflow(s.overflow.1),
        },
        box_sizing: match s.box_sizing {
            BoxSizing::BorderBox => tf::BoxSizing::BorderBox,
            BoxSizing::ContentBox => tf::BoxSizing::ContentBox,
        },
        grid_template_columns: s
            .grid_template_columns
            .iter()
            .map(|t| match t {
                Track::Px(v) => tf::length(*v),
                Track::Fr(v) => tf::fr(*v),
                Track::Auto => tf::auto(),
            })
            .collect(),
        ..Default::default()
    }
}
fn legacy_style(b: Bounds) -> tf::Style {
    tf::Style {
        position: tf::Position::Relative,
        flex_shrink: 0.0,
        size: tf::Size {
            width: tf::Dimension::Length(b.width),
            height: tf::Dimension::Length(b.height),
        },
        ..Default::default()
    }
}
fn dim(v: LayoutValue) -> tf::Dimension {
    match v {
        LayoutValue::Px(v) => tf::Dimension::Length(v),
        LayoutValue::Pct(v) => tf::Dimension::Percent(v),
        LayoutValue::Auto => tf::Dimension::Auto,
    }
}
fn length(v: LayoutValue) -> tf::LengthPercentage {
    match v {
        LayoutValue::Px(v) => tf::LengthPercentage::Length(v),
        LayoutValue::Pct(v) => tf::LengthPercentage::Percent(v),
        LayoutValue::Auto => tf::LengthPercentage::Length(0.0),
    }
}
fn auto(v: LayoutValue) -> tf::LengthPercentageAuto {
    match v {
        LayoutValue::Px(v) => tf::LengthPercentageAuto::Length(v),
        LayoutValue::Pct(v) => tf::LengthPercentageAuto::Percent(v),
        LayoutValue::Auto => tf::LengthPercentageAuto::Auto,
    }
}
fn rect_len(e: Edges<LayoutValue>) -> tf::Rect<tf::LengthPercentage> {
    tf::Rect {
        top: length(e.top),
        right: length(e.right),
        bottom: length(e.bottom),
        left: length(e.left),
    }
}
fn rect_auto(e: Edges<LayoutValue>) -> tf::Rect<tf::LengthPercentageAuto> {
    tf::Rect {
        top: auto(e.top),
        right: auto(e.right),
        bottom: auto(e.bottom),
        left: auto(e.left),
    }
}
fn map_align(v: AlignItems) -> tf::AlignItems {
    match v {
        AlignItems::Start => tf::AlignItems::FlexStart,
        AlignItems::End => tf::AlignItems::FlexEnd,
        AlignItems::Center => tf::AlignItems::Center,
        AlignItems::Stretch => tf::AlignItems::Stretch,
        AlignItems::Baseline => tf::AlignItems::Baseline,
    }
}
fn map_justify(v: JustifyContent) -> tf::JustifyContent {
    match v {
        JustifyContent::Start => tf::JustifyContent::FlexStart,
        JustifyContent::End => tf::JustifyContent::FlexEnd,
        JustifyContent::Center => tf::JustifyContent::Center,
        JustifyContent::SpaceBetween => tf::JustifyContent::SpaceBetween,
        JustifyContent::SpaceAround => tf::JustifyContent::SpaceAround,
        JustifyContent::SpaceEvenly => tf::JustifyContent::SpaceEvenly,
    }
}
fn map_overflow(v: Overflow) -> TaffyOverflow {
    match v {
        Overflow::Visible => TaffyOverflow::Visible,
        Overflow::Hidden => TaffyOverflow::Hidden,
    }
}

fn write_layout(
    tree: &LayoutTree,
    node: &mut Node,
    ids: &HashMap<NodeId, tf::NodeId>,
    origin: (f32, f32),
) {
    let l = tree.layout(ids[&node.id]).expect("computed");
    let here = (origin.0 + l.location.x, origin.1 + l.location.y);
    node.bounds.x = here.0;
    node.bounds.y = here.1;
    node.bounds.width = l.size.width;
    node.bounds.height = l.size.height;
    // A legacy island participates in its parent's flow, but its descendants remain
    // an opaque legacy subtree whose explicit bounds are authoritative.
    if node.layout.is_none() {
        return;
    }
    for child in &mut node.children {
        write_layout(tree, child, ids, here)
    }
}
fn write_root_layout(
    tree: &LayoutTree,
    root: &mut Node,
    ids: &HashMap<NodeId, tf::NodeId>,
    authored_origin: (f32, f32),
    safe_insets: Insets,
) {
    let layout = tree.layout(ids[&root.id]).expect("computed");
    root.bounds.x = authored_origin.0;
    root.bounds.y = authored_origin.1;
    root.bounds.width = layout.size.width;
    root.bounds.height = layout.size.height;

    let interior_origin = (
        authored_origin.0 + safe_insets.left,
        authored_origin.1 + safe_insets.top,
    );
    for child in &mut root.children {
        write_layout(tree, child, ids, interior_origin)
    }
}
fn collect_bounds(n: &Node, origin: (f32, f32), out: &mut Vec<(NodeId, Bounds)>) {
    let mut relative = n.bounds;
    relative.x -= origin.0;
    relative.y -= origin.1;
    out.push((n.id.clone(), relative));
    for c in &n.children {
        collect_bounds(c, origin, out)
    }
}
fn apply_bounds(n: &mut Node, b: &[(NodeId, Bounds)], origin: (f32, f32)) {
    if let Some((_, v)) = b.iter().find(|(id, _)| id == &n.id) {
        n.bounds = Bounds {
            x: v.x + origin.0,
            y: v.y + origin.1,
            ..*v
        };
    }
    for c in &mut n.children {
        apply_bounds(c, b, origin)
    }
}
fn input_hash(root: &Node) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    hash_node(root, &mut h, true);
    h.finish()
}
fn hash_node(n: &Node, h: &mut impl Hasher, participating: bool) {
    n.id.hash(h);
    n.accessible_label.hash(h);
    n.content.hash(h);
    n.type_role.hash(h);
    std::mem::discriminant(&n.text_align).hash(h);
    n.role.hash(h);
    n.line_height.map(f32::to_bits).hash(h);
    hash_style(n.layout.as_ref(), h);
    if n.layout.is_none() {
        let values: &[f32] = if participating {
            // A legacy island's position is a layout output; only its authored size is input.
            &[n.bounds.width, n.bounds.height]
        } else {
            // Descendants of a legacy island are opaque and keep all authored bounds.
            &[n.bounds.x, n.bounds.y, n.bounds.width, n.bounds.height]
        };
        for value in values {
            value.to_bits().hash(h);
        }
    }
    for c in &n.children {
        hash_node(c, h, participating && n.layout.is_some())
    }
}
fn hash_style(s: Option<&LayoutStyle>, h: &mut impl Hasher) {
    format!("{s:?}").hash(h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Insets, Orientation, Role};

    struct Measure;
    impl TextMeasure for Measure {
        fn measure(
            &self,
            text: &str,
            role: TypeRole,
            _align: TextAlign,
            scale: f32,
            max: Option<f32>,
            line_height: Option<f32>,
        ) -> (f32, f32) {
            let role_scale = match role {
                TypeRole::Hero => 2.0,
                _ => 1.0,
            };
            let content_width = text.chars().map(u32::from).sum::<u32>() as f32;
            (
                (content_width * role_scale * scale).min(max.unwrap_or(f32::MAX)),
                10.0 * role_scale * scale * line_height.unwrap_or(1.0),
            )
        }
    }
    fn node(id: &str, bounds: Bounds) -> Node {
        Node::new(NodeId::new(id).unwrap(), Role::Group, id, bounds, "surface")
    }
    fn metrics(width: f32) -> Metrics {
        Metrics {
            logical_width: width,
            logical_height: 240.0,
            scale: 1.0,
            safe_insets: Insets::default(),
            orientation: Orientation::Landscape,
        }
    }

    #[test]
    fn no_layout_is_strictly_untouched() {
        let mut root = node("root", Bounds::new(3.0, 4.0, 100.0, 80.0))
            .with_children(vec![node("child", Bounds::new(9.0, 7.0, 12.0, 14.0))]);
        let before = format!("{root:?}");
        resolve_layout(
            &mut root,
            metrics(320.0),
            1.0,
            &Measure,
            &mut LayoutCache::default(),
        );
        assert_eq!(format!("{root:?}"), before);
    }

    #[test]
    fn legacy_island_keeps_explicit_size_and_uses_flow_position() {
        let island = node("island", Bounds::new(17.0, 23.0, 31.0, 41.0));
        let style = LayoutStyle {
            width: LayoutValue::Px(200.0),
            height: LayoutValue::Px(100.0),
            ..LayoutStyle::default()
        };
        let mut root = node("root", Bounds::new(5.0, 6.0, 1.0, 1.0))
            .with_layout(style)
            .with_children(vec![island]);
        resolve_layout(
            &mut root,
            metrics(320.0),
            1.0,
            &Measure,
            &mut LayoutCache::default(),
        );
        assert_eq!(root.children[0].bounds, Bounds::new(5.0, 6.0, 31.0, 41.0));
    }

    #[test]
    fn safe_insets_reduce_available_space_and_offset_the_subtree_interior_idempotently() {
        let style = LayoutStyle {
            width: LayoutValue::Pct(1.0),
            height: LayoutValue::Pct(1.0),
            ..LayoutStyle::default()
        };
        let child = node("child", Bounds::new(0.0, 0.0, 10.0, 20.0));
        let scene = node("root", Bounds::new(7.0, 11.0, 0.0, 0.0))
            .with_layout(style)
            .with_children(vec![child]);
        let mut without_insets = scene.clone();
        let mut with_insets = scene;
        let mut inset_metrics = metrics(320.0);
        inset_metrics.safe_insets = Insets {
            top: 13.0,
            right: 17.0,
            bottom: 19.0,
            left: 23.0,
        };

        resolve_layout(
            &mut without_insets,
            metrics(320.0),
            1.0,
            &Measure,
            &mut LayoutCache::default(),
        );
        let mut cache = LayoutCache::default();
        resolve_layout(&mut with_insets, inset_metrics, 1.0, &Measure, &mut cache);

        assert_eq!(without_insets.bounds, Bounds::new(7.0, 11.0, 320.0, 240.0));
        assert_eq!(with_insets.bounds, Bounds::new(7.0, 11.0, 280.0, 208.0));
        assert_eq!(
            with_insets.children[0].bounds,
            Bounds::new(30.0, 24.0, 10.0, 20.0)
        );

        let first_bounds = format!("{:?}", with_insets);
        resolve_layout(&mut with_insets, inset_metrics, 1.0, &Measure, &mut cache);
        assert_eq!(format!("{:?}", with_insets), first_bounds);
        assert_eq!(
            cache.hits(),
            1,
            "an unchanged re-resolve must hit the cache"
        );
    }

    #[test]
    fn nested_subtree_uses_enclosing_legacy_box_without_surface_insets_and_cache_staleness() {
        let nested = node("nested", Bounds::new(0.0, 0.0, 0.0, 0.0)).with_layout(LayoutStyle {
            width: LayoutValue::Pct(1.0),
            height: LayoutValue::Pct(1.0),
            ..LayoutStyle::default()
        });
        let parent =
            node("parent", Bounds::new(41.0, 53.0, 100.0, 80.0)).with_children(vec![nested]);
        let mut root =
            node("root", Bounds::new(0.0, 0.0, 320.0, 240.0)).with_children(vec![parent]);
        let mut surface = metrics(320.0);
        surface.safe_insets = Insets {
            top: 13.0,
            right: 17.0,
            bottom: 19.0,
            left: 23.0,
        };
        let mut cache = LayoutCache::default();

        resolve_layout(&mut root, surface, 1.0, &Measure, &mut cache);
        assert_eq!(
            root.children[0].children[0].bounds,
            Bounds::new(41.0, 53.0, 100.0, 80.0)
        );
        assert_eq!(cache.hits(), 0);

        root.children[0].bounds = Bounds::new(61.0, 71.0, 120.0, 90.0);
        resolve_layout(&mut root, surface, 1.0, &Measure, &mut cache);
        assert_eq!(
            root.children[0].children[0].bounds,
            Bounds::new(61.0, 71.0, 120.0, 90.0),
            "the enclosing box must participate in the nested-root cache key"
        );
        assert_eq!(
            cache.hits(),
            0,
            "a resized enclosing box must miss the cache"
        );

        resolve_layout(&mut root, surface, 1.0, &Measure, &mut cache);
        assert_eq!(
            cache.hits(),
            1,
            "an unchanged enclosing box must hit the cache"
        );
    }

    #[test]
    fn cached_nested_subtree_reanchors_when_enclosing_legacy_box_moves() {
        let nested = node("nested", Bounds::new(0.0, 0.0, 0.0, 0.0)).with_layout(LayoutStyle {
            width: LayoutValue::Pct(1.0),
            height: LayoutValue::Pct(1.0),
            ..LayoutStyle::default()
        });
        let parent =
            node("parent", Bounds::new(41.0, 53.0, 100.0, 80.0)).with_children(vec![nested]);
        let mut root =
            node("root", Bounds::new(0.0, 0.0, 320.0, 240.0)).with_children(vec![parent]);
        let mut cache = LayoutCache::default();

        resolve_layout(&mut root, metrics(320.0), 1.0, &Measure, &mut cache);
        assert_eq!(
            root.children[0].children[0].bounds,
            Bounds::new(41.0, 53.0, 100.0, 80.0)
        );
        assert_eq!(cache.hits(), 0);

        root.children[0].bounds = Bounds::new(61.0, 71.0, 100.0, 80.0);
        resolve_layout(&mut root, metrics(320.0), 1.0, &Measure, &mut cache);

        assert_eq!(
            cache.hits(),
            1,
            "moving an unchanged enclosing box must hit the cache"
        );
        assert_eq!(
            root.children[0].children[0].bounds,
            Bounds::new(61.0, 71.0, 100.0, 80.0),
            "cached bounds must re-anchor to the current enclosing origin"
        );
    }

    #[test]
    fn legacy_island_reserves_space_between_migrated_flex_siblings() {
        let zero_edges = Edges {
            top: LayoutValue::Px(0.0),
            right: LayoutValue::Px(0.0),
            bottom: LayoutValue::Px(0.0),
            left: LayoutValue::Px(0.0),
        };
        let fixed = |width| LayoutStyle {
            width: LayoutValue::Px(width),
            height: LayoutValue::Px(20.0),
            flex_shrink: 0.0,
            margin: zero_edges,
            ..LayoutStyle::default()
        };
        let first = node("first", Bounds::new(0.0, 0.0, 0.0, 0.0)).with_layout(fixed(40.0));
        let island = node("island", Bounds::new(99.0, 77.0, 30.0, 20.0));
        let third = node("third", Bounds::new(0.0, 0.0, 0.0, 0.0)).with_layout(fixed(50.0));
        let mut root = node("root", Bounds::new(0.0, 0.0, 0.0, 0.0))
            .with_layout(LayoutStyle {
                width: LayoutValue::Px(200.0),
                height: LayoutValue::Px(20.0),
                margin: zero_edges,
                ..LayoutStyle::default()
            })
            .with_children(vec![first, island, third]);

        resolve_layout(
            &mut root,
            metrics(320.0),
            1.0,
            &Measure,
            &mut LayoutCache::default(),
        );

        assert_eq!(root.children[0].bounds.x, 0.0);
        assert_eq!(root.children[1].bounds, Bounds::new(40.0, 0.0, 30.0, 20.0));
        assert_eq!(root.children[2].bounds.x, 70.0);
    }

    #[test]
    fn migrated_child_inside_flowed_legacy_island_uses_resolved_island_box() {
        let zero_edges = Edges {
            top: LayoutValue::Px(0.0),
            right: LayoutValue::Px(0.0),
            bottom: LayoutValue::Px(0.0),
            left: LayoutValue::Px(0.0),
        };
        let nested = node("nested", Bounds::new(0.0, 0.0, 0.0, 0.0)).with_layout(LayoutStyle {
            width: LayoutValue::Pct(1.0),
            height: LayoutValue::Pct(1.0),
            margin: zero_edges,
            ..LayoutStyle::default()
        });
        let island =
            node("island", Bounds::new(99.0, 77.0, 60.0, 30.0)).with_children(vec![nested]);
        let spacer = node("spacer", Bounds::new(0.0, 0.0, 0.0, 0.0)).with_layout(LayoutStyle {
            width: LayoutValue::Px(40.0),
            height: LayoutValue::Px(30.0),
            flex_shrink: 0.0,
            margin: zero_edges,
            ..LayoutStyle::default()
        });
        let mut root = node("root", Bounds::new(5.0, 7.0, 0.0, 0.0))
            .with_layout(LayoutStyle {
                width: LayoutValue::Px(200.0),
                height: LayoutValue::Px(30.0),
                flex_direction: FlexDirection::RowReverse,
                margin: zero_edges,
                ..LayoutStyle::default()
            })
            .with_children(vec![spacer, island]);
        let mut cache = LayoutCache::default();

        resolve_layout(&mut root, metrics(320.0), 1.0, &Measure, &mut cache);

        assert_eq!(root.children[1].bounds, Bounds::new(105.0, 7.0, 60.0, 30.0));
        assert_eq!(
            root.children[1].children[0].bounds,
            Bounds::new(105.0, 7.0, 60.0, 30.0)
        );

        root.layout.as_mut().unwrap().flex_direction = FlexDirection::Row;
        resolve_layout(&mut root, metrics(320.0), 1.0, &Measure, &mut cache);
        assert_eq!(
            cache.hits(),
            1,
            "the unchanged nested subtree must hit its cache"
        );
        assert_eq!(
            root.children[1].children[0].bounds,
            Bounds::new(45.0, 7.0, 60.0, 30.0),
            "the cache hit must mount at the island's reflowed origin"
        );
    }

    #[test]
    fn migrated_child_resolves_through_two_nested_legacy_islands() {
        let nested = node("nested", Bounds::new(0.0, 0.0, 0.0, 0.0)).with_layout(LayoutStyle {
            width: LayoutValue::Pct(1.0),
            height: LayoutValue::Pct(1.0),
            ..LayoutStyle::default()
        });
        let inner = node("inner", Bounds::new(13.0, 11.0, 50.0, 20.0)).with_children(vec![nested]);
        let outer = node("outer", Bounds::new(0.0, 0.0, 70.0, 30.0)).with_children(vec![inner]);
        let spacer = node("spacer", Bounds::new(0.0, 0.0, 0.0, 0.0)).with_layout(LayoutStyle {
            width: LayoutValue::Px(40.0),
            height: LayoutValue::Px(30.0),
            flex_shrink: 0.0,
            ..LayoutStyle::default()
        });
        let mut root = node("root", Bounds::new(5.0, 7.0, 0.0, 0.0))
            .with_layout(LayoutStyle {
                width: LayoutValue::Px(200.0),
                height: LayoutValue::Px(30.0),
                flex_direction: FlexDirection::RowReverse,
                ..LayoutStyle::default()
            })
            .with_children(vec![spacer, outer]);
        resolve_layout(
            &mut root,
            metrics(320.0),
            1.0,
            &Measure,
            &mut LayoutCache::default(),
        );
        assert_eq!(
            root.children[1].children[0].children[0].bounds,
            Bounds::new(13.0, 11.0, 50.0, 20.0),
            "walking legacy islands must recurse to participating roots at any depth"
        );
    }

    #[test]
    fn migrated_child_resolves_in_legacy_island_below_styled_child_without_reresolve() {
        let descendant =
            node("descendant", Bounds::new(0.0, 0.0, 0.0, 0.0)).with_layout(LayoutStyle {
                width: LayoutValue::Pct(1.0),
                height: LayoutValue::Pct(1.0),
                ..LayoutStyle::default()
            });
        let island =
            node("island", Bounds::new(0.0, 0.0, 60.0, 20.0)).with_children(vec![descendant]);
        let styled = node("styled", Bounds::new(0.0, 0.0, 0.0, 0.0))
            .with_layout(LayoutStyle {
                width: LayoutValue::Px(120.0),
                height: LayoutValue::Px(40.0),
                ..LayoutStyle::default()
            })
            .with_children(vec![island]);
        let mut root = node("root", Bounds::new(5.0, 7.0, 0.0, 0.0))
            .with_layout(LayoutStyle {
                width: LayoutValue::Px(200.0),
                height: LayoutValue::Px(80.0),
                ..LayoutStyle::default()
            })
            .with_children(vec![styled]);
        let mut cache = LayoutCache::default();

        resolve_layout(&mut root, metrics(320.0), 1.0, &Measure, &mut cache);

        let island = &root.children[0].children[0];
        assert_eq!(island.children[0].bounds, island.bounds);
        assert_eq!(
            cache.entries.len(),
            2,
            "only the outer subtree and the descendant below the island are fresh roots"
        );
    }

    #[test]
    fn style_inventory_maps_to_taffy() {
        let s = LayoutStyle {
            display: Display::Grid,
            position: Position::Absolute,
            flex_direction: FlexDirection::ColumnReverse,
            align_items: Some(AlignItems::Center),
            justify_content: Some(JustifyContent::SpaceEvenly),
            flex_grow: 2.0,
            flex_shrink: 3.0,
            flex_basis: LayoutValue::Pct(0.25),
            gap: (LayoutValue::Px(4.0), LayoutValue::Pct(0.1)),
            padding: Edges {
                top: LayoutValue::Px(1.0),
                right: LayoutValue::Px(2.0),
                bottom: LayoutValue::Px(3.0),
                left: LayoutValue::Px(4.0),
            },
            margin: Edges {
                left: LayoutValue::Auto,
                ..Edges::default()
            },
            width: LayoutValue::Px(80.0),
            height: LayoutValue::Pct(0.5),
            min_width: LayoutValue::Px(10.0),
            min_height: LayoutValue::Px(11.0),
            max_width: LayoutValue::Px(90.0),
            max_height: LayoutValue::Px(91.0),
            inset: Edges {
                top: LayoutValue::Px(7.0),
                ..Edges::default()
            },
            overflow: (Overflow::Hidden, Overflow::Visible),
            box_sizing: BoxSizing::ContentBox,
            grid_template_columns: vec![Track::Px(10.0), Track::Fr(1.0), Track::Auto],
        };
        let t = to_taffy(&s);
        assert_eq!(t.display, tf::Display::Grid);
        assert_eq!(t.position, tf::Position::Absolute);
        assert_eq!(t.flex_direction, tf::FlexDirection::ColumnReverse);
        assert_eq!(t.align_items, Some(tf::AlignItems::Center));
        assert_eq!(t.justify_content, Some(tf::JustifyContent::SpaceEvenly));
        assert_eq!(t.flex_grow, 2.0);
        assert_eq!(t.flex_shrink, 3.0);
        assert_eq!(t.flex_basis, tf::Dimension::Percent(0.25));
        assert_eq!(t.size.width, tf::Dimension::Length(80.0));
        assert_eq!(t.size.height, tf::Dimension::Percent(0.5));
        assert_eq!(t.min_size.width, tf::Dimension::Length(10.0));
        assert_eq!(t.max_size.width, tf::Dimension::Length(90.0));
        assert_eq!(t.max_size.height, tf::Dimension::Length(91.0));
        assert_eq!(t.padding.left, tf::LengthPercentage::Length(4.0));
        assert_eq!(t.margin.left, tf::LengthPercentageAuto::Auto);
        assert_eq!(t.inset.top, tf::LengthPercentageAuto::Length(7.0));
        assert_eq!(t.overflow.x, TaffyOverflow::Hidden);
        assert_eq!(t.box_sizing, tf::BoxSizing::ContentBox);
        assert_eq!(t.grid_template_columns.len(), 3);
    }

    #[test]
    fn max_height_caps_taller_measured_content() {
        let style = LayoutStyle {
            max_height: LayoutValue::Px(25.0),
            ..LayoutStyle::default()
        };
        let mut root = node("root", Bounds::new(0.0, 0.0, 0.0, 0.0)).with_layout(style);
        root.role = Role::Text;
        root.line_height = Some(4.0);

        resolve_layout(
            &mut root,
            metrics(320.0),
            1.0,
            &Measure,
            &mut LayoutCache::default(),
        );

        assert!(
            root.bounds.height <= 25.0,
            "resolved bounds: {:?}",
            root.bounds
        );
    }

    #[test]
    fn cache_is_sensitive_and_deterministic() {
        fn laid_out(
            width: f32,
            scale: f32,
            text: &str,
            revision: u64,
            cache: &mut LayoutCache,
        ) -> Bounds {
            let s = LayoutStyle {
                width: LayoutValue::Pct(1.0),
                ..LayoutStyle::default()
            };
            let mut root = node("root", Bounds::new(0.0, 0.0, 0.0, 0.0)).with_layout(s);
            root.accessible_label = text.into();
            cache.set_typography_revision(revision);
            resolve_layout(&mut root, metrics(width), scale, &Measure, cache);
            root.bounds
        }
        let mut cache = LayoutCache::default();
        let first = laid_out(100.0, 1.0, "a", 1, &mut cache);
        assert_eq!(cache.hits(), 0);
        assert_eq!(laid_out(100.0, 1.0, "a", 1, &mut cache), first);
        assert_eq!(cache.hits(), 1);
        laid_out(101.0, 1.0, "a", 1, &mut cache);
        laid_out(101.0, 2.0, "a", 1, &mut cache);
        laid_out(101.0, 2.0, "longer", 1, &mut cache);
        assert_eq!(cache.hits(), 1);
        laid_out(101.0, 2.0, "longer", 2, &mut cache);
        assert_eq!(cache.hits(), 1);
        let mut fresh_a = LayoutCache::default();
        let mut fresh_b = LayoutCache::default();
        assert_eq!(
            laid_out(123.0, 1.25, "deterministic", 4, &mut fresh_a),
            laid_out(123.0, 1.25, "deterministic", 4, &mut fresh_b)
        );
    }

    #[test]
    fn cache_restore_preserves_bounds_constraints() {
        let style = LayoutStyle {
            width: LayoutValue::Px(80.0),
            height: LayoutValue::Px(40.0),
            ..LayoutStyle::default()
        };
        let constraints = (12.0, 14.0, Some(120.0), Some(140.0));
        let mut bounds = Bounds::new(3.0, 4.0, 0.0, 0.0);
        bounds.min_width = constraints.0;
        bounds.min_height = constraints.1;
        bounds.max_width = constraints.2;
        bounds.max_height = constraints.3;
        let mut root = node("root", bounds).with_layout(style);
        let mut cache = LayoutCache::default();

        resolve_layout(&mut root, metrics(320.0), 1.0, &Measure, &mut cache);
        assert_eq!(
            (
                root.bounds.min_width,
                root.bounds.min_height,
                root.bounds.max_width,
                root.bounds.max_height,
            ),
            constraints,
            "a cache miss must preserve legacy bounds constraints"
        );

        resolve_layout(&mut root, metrics(320.0), 1.0, &Measure, &mut cache);
        assert_eq!(cache.hits(), 1, "the second resolve must hit the cache");
        assert_eq!(
            (
                root.bounds.min_width,
                root.bounds.min_height,
                root.bounds.max_width,
                root.bounds.max_height,
            ),
            constraints,
            "a cache hit must preserve legacy bounds constraints"
        );
    }

    #[test]
    fn cache_tracks_measurement_selecting_node_fields() {
        let mut cache = LayoutCache::default();
        let mut root =
            node("root", Bounds::new(0.0, 0.0, 0.0, 0.0)).with_layout(LayoutStyle::default());
        root.role = Role::Text;
        root.accessible_label = "az".into();

        resolve_layout(&mut root, metrics(1_000.0), 1.0, &Measure, &mut cache);
        let initial = root.bounds;
        resolve_layout(&mut root, metrics(1_000.0), 1.0, &Measure, &mut cache);
        assert_eq!(root.bounds, initial);
        assert_eq!(cache.hits(), 1, "an unchanged resolve must hit the cache");

        root.accessible_label = "ay".into();
        resolve_layout(&mut root, metrics(1_000.0), 1.0, &Measure, &mut cache);
        assert_ne!(
            root.bounds, initial,
            "same-length label content must remeasure"
        );
        assert_eq!(cache.hits(), 1, "changed label content must miss the cache");

        let relabeled = root.bounds;
        root.type_role = TypeRole::Hero;
        resolve_layout(&mut root, metrics(1_000.0), 1.0, &Measure, &mut cache);
        assert_ne!(root.bounds, relabeled, "a type-role change must remeasure");
        assert_eq!(cache.hits(), 1, "changed type role must miss the cache");

        let retyped = root.bounds;
        root.line_height = Some(1.5);
        resolve_layout(&mut root, metrics(1_000.0), 1.0, &Measure, &mut cache);
        assert_ne!(root.bounds, retyped, "a line-height change must remeasure");
        assert_eq!(cache.hits(), 1, "changed line height must miss the cache");
    }
}
