//! Deterministic scene rasterization on the ruled Cosmic Text/Swash/tiny-skia stack.
//!
//! All paint colors are resolved from the active `pf-theme` base at presentation time.

use cosmic_text_tracking as tracked_text;
use pf_scene::{
    Bounds, Elevation, ImageFit, ImageSource, Node, NodeContent, Role, Scene, SurfaceMetrics,
    TypeRole,
};
pub use pf_theme::Base as ThemeBase;
use pf_theme::{ResolvedStyleSnapshot, Rgba};
use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use tiny_skia::{
    Color as SkColor, FilterQuality, Mask, Paint, PathBuilder, Pixmap, PixmapPaint, Rect, Stroke,
    StrokeDash, Transform,
};

const MANROPE: &[u8] =
    include_bytes!("../../../spikes/render-text/fonts/manrope/Manrope[wght].ttf");
const FRAUNCES: &[u8] =
    include_bytes!("../../../spikes/render-text/fonts/fraunces/Fraunces[SOFT,WONK,opsz,wght].ttf");
const CJK: &[u8] = include_bytes!("../fonts/NotoSansCJK-Regular.ttc");
/// Maximum decoded PNG dimensions accepted by the rasterizer (8 megapixels).
pub const MAX_IMAGE_PIXELS: u64 = 8_000_000;
const IMAGE_CACHE_CAPACITY: usize = 16;
const ROOT_FONT_SIZE: f32 = 16.0;
const TYPE_TOKENS: &str = include_str!("../../pf-theme/vendor/package/tokens.json");

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedTypeStyle {
    pub family: String,
    pub size_px: f32,
    pub weight: u16,
    pub tracking_em: f32,
}

#[derive(Clone, Debug)]
struct TypographySnapshot {
    roles: HashMap<TypeRole, ResolvedTypeStyle>,
}

impl TypographySnapshot {
    fn flagship() -> Self {
        let roles = [
            TypeRole::Hero,
            TypeRole::Title,
            TypeRole::H1,
            TypeRole::Body,
            TypeRole::Label,
            TypeRole::Caption,
            TypeRole::Eyebrow,
            TypeRole::Plate,
        ]
        .into_iter()
        .map(|role| (role, parse_type_role(role)))
        .collect();
        Self { roles }
    }

    fn resolve(&self, role: TypeRole) -> &ResolvedTypeStyle {
        &self.roles[&role]
    }
}

fn parse_type_role(role: TypeRole) -> ResolvedTypeStyle {
    let theme = serde_json::from_str::<serde_json::Value>(TYPE_TOKENS)
        .expect("embedded flagship type tokens are valid JSON");
    let values = theme["theme"].as_object().expect("theme token object");
    if role == TypeRole::Plate {
        return ResolvedTypeStyle {
            family: token(values, "--type-family-plate").into(),
            size_px: ROOT_FONT_SIZE,
            weight: 500,
            tracking_em: 0.0,
        };
    }
    let key = match role {
        TypeRole::Hero => "hero",
        TypeRole::Title => "title",
        TypeRole::H1 => "h1",
        TypeRole::Body => "body",
        TypeRole::Label => "label",
        TypeRole::Caption => "caption",
        TypeRole::Eyebrow => "eyebrow",
        TypeRole::Plate => unreachable!(),
    };
    let family_key = if matches!(role, TypeRole::Hero | TypeRole::Title) {
        "--type-family-display"
    } else {
        "--type-family-ui"
    };
    let size = token(values, &format!("--type-{key}-size"));
    let weight = token(values, &format!("--type-{key}-weight"));
    ResolvedTypeStyle {
        family: token(values, family_key).into(),
        size_px: size
            .strip_suffix("rem")
            .expect("type size is rem")
            .parse::<f32>()
            .expect("numeric type size")
            * ROOT_FONT_SIZE,
        weight: weight.parse().expect("numeric type weight"),
        tracking_em: if role == TypeRole::Eyebrow {
            token(values, "--type-eyebrow-tracking")
                .strip_suffix("em")
                .expect("tracking is em")
                .parse()
                .expect("numeric tracking")
        } else {
            0.0
        },
    }
}

fn token<'a>(values: &'a serde_json::Map<String, serde_json::Value>, key: &str) -> &'a str {
    values[key].as_str().expect("string type token")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DamageRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl DamageRect {
    fn union(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = (self.x + self.width).max(other.x + other.width);
        let bottom = (self.y + self.height).max(other.y + other.height);
        Self {
            x,
            y,
            width: right - x,
            height: bottom - y,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RasterFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub damage: Option<DamageRect>,
    /// Recoverable image failures encountered while producing this frame.
    pub notes: Vec<RenderNote>,
}

/// A typed, non-fatal condition for which the embedder may choose a fallback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RenderNote {
    ImageDecodeFailed {
        source_id: String,
    },
    ImageTooLarge {
        source_id: String,
        width: u32,
        height: u32,
        max_pixels: u64,
    },
}

/// Long-lived rasterizer. Cosmic Text's shaping state and Swash's glyph images are retained.
pub struct Rasterizer {
    fonts: tracked_text::FontSystem,
    glyphs: tracked_text::SwashCache,
    previous: Vec<NodeSnapshot>,
    images: ImageCache,
    rounded_shadows: RoundedShadowCache,
    theme_base: ThemeBase,
    style: ResolvedStyleSnapshot,
    typography: TypographySnapshot,
    text_scale: f32,
}

struct ShadowAsset {
    base: ThemeBase,
    elevation: Elevation,
    side: usize,
    margin: usize,
    offset_x: f32,
    offset_y: f32,
    blur: f32,
    spread: f32,
    color: [u8; 4],
    rgba: &'static [u8],
}

struct RoundedShadowAsset {
    width: usize,
    height: usize,
    slice_margin_x: usize,
    slice_margin_y: usize,
    effect_margin: usize,
    rgba: Vec<u8>,
}

const ROUNDED_SHADOW_CACHE_CAPACITY: usize = 32;
// Large enough for handheld UI pills, while keeping one RGBA bake below 2 MiB and the full cache
// below 64 MiB even when hostile content churns through maximum-sized radii and effect margins.
const MAX_ROUNDED_SHADOW_RADIUS: u32 = 256;

#[derive(Default)]
struct RoundedShadowCache {
    assets: HashMap<(u8, u8, u32, u32, u16), RoundedShadowAsset>,
    recency: VecDeque<(u8, u8, u32, u32, u16)>,
}

impl RoundedShadowCache {
    fn get_or_bake(
        &mut self,
        key: (u8, u8, u32, u32, u16),
        asset: &ShadowAsset,
    ) -> &RoundedShadowAsset {
        if let Some(position) = self.recency.iter().position(|candidate| *candidate == key) {
            self.recency.remove(position);
        } else {
            if self.assets.len() == ROUNDED_SHADOW_CACHE_CAPACITY {
                let oldest = self
                    .recency
                    .pop_front()
                    .expect("a full shadow cache has an oldest entry");
                self.assets.remove(&oldest);
            }
            self.assets.insert(
                key,
                bake_rounded_shadow_physical(
                    asset,
                    Radii::new(key.2 as f32, key.3 as f32),
                    key.4 as usize,
                ),
            );
        }
        self.recency.push_back(key);
        self.assets
            .get(&key)
            .expect("requested shadow is present after cache update")
    }
}

const SHADOW_ASSETS: &[ShadowAsset] = include!(concat!(env!("OUT_DIR"), "/shadow_assets.rs"));

#[derive(Clone, PartialEq)]
struct NodeSnapshot {
    id: String,
    parent_id: Option<String>,
    sibling_index: usize,
    bounds: Bounds,
    /// Participates in damage tracking exactly when the accessible label is painted:
    /// text/heading role plus `NodeContent::Label`.
    label: String,
    style_token: String,
    type_role: TypeRole,
    line_height: Option<f32>,
    corner_radius: f32,
    focused: bool,
    pressed: bool,
    disabled: bool,
    selected: bool,
    unavailable: bool,
    destructive: bool,
    scrimmed: bool,
    elevation: Elevation,
    content: ContentSnapshot,
}

#[derive(Clone, PartialEq)]
enum ContentSnapshot {
    Label,
    Image { source_id: String, fit: ImageFit },
}

#[derive(Default)]
struct ImageCache {
    decoded: HashMap<String, Pixmap>,
    insertion_order: VecDeque<String>,
}

impl Rasterizer {
    pub fn new() -> Self {
        let mut db = tracked_text::fontdb::Database::new();
        // Never call load_system_fonts: output must depend only on repository bytes.
        for data in [MANROPE, FRAUNCES, CJK] {
            db.load_font_data(data.to_vec());
        }
        Self {
            fonts: tracked_text::FontSystem::new_with_locale_and_db("en-US".into(), db),
            glyphs: tracked_text::SwashCache::new(),
            previous: Vec::new(),
            images: ImageCache::default(),
            rounded_shadows: RoundedShadowCache::default(),
            theme_base: ThemeBase::Dusk,
            style: pf_theme::flagship()
                .resolved_style(ThemeBase::Dusk)
                .expect("embedded dusk style snapshot"),
            typography: TypographySnapshot::flagship(),
            text_scale: 1.0,
        }
    }

    /// Sets accessibility text scale. Text is reshaped and reflowed at this size.
    pub fn set_text_scale(&mut self, factor: f32) -> Result<(), RenderError> {
        if !factor.is_finite() || factor <= 0.0 {
            return Err(RenderError::InvalidTextScale);
        }
        if self.text_scale != factor {
            self.text_scale = factor;
            self.previous.clear();
        }
        Ok(())
    }

    pub fn set_theme_base(&mut self, base: ThemeBase) {
        if self.theme_base != base {
            self.style = pf_theme::flagship()
                .resolved_style(base)
                .expect("embedded theme base is complete and typed");
            self.theme_base = base;
            self.previous.clear();
        }
    }

    pub fn render(
        &mut self,
        scene: &Scene,
        metrics: SurfaceMetrics,
    ) -> Result<RasterFrame, RenderError> {
        let width = physical(metrics.logical_width, metrics.scale)?;
        let height = physical(metrics.logical_height, metrics.scale)?;
        let mut pixmap = Pixmap::new(width, height).ok_or(RenderError::InvalidSurface)?;
        let background = style_color(&self.style, "--color-surface-canvas")?;
        pixmap.fill(SkColor::from_rgba8(
            background.red,
            background.green,
            background.blue,
            background.alpha,
        ));
        let mut notes = Vec::new();
        let mut context = DrawContext {
            fonts: &mut self.fonts,
            glyphs: &mut self.glyphs,
            images: &mut self.images,
            rounded_shadows: &mut self.rounded_shadows,
            notes: &mut notes,
            style: &self.style,
            typography: &self.typography,
            text_scale: self.text_scale,
        };
        draw_node(
            &mut pixmap,
            &mut context,
            scene.root(),
            metrics.scale,
            LogicalTransform::IDENTITY,
        )?;
        let mut current = Vec::new();
        let pressed_shift = self
            .style
            .length("--state-pressed-shift")
            .expect("typed pressed shift")
            .pixels;
        collect(
            scene.root(),
            None,
            0,
            &mut current,
            LogicalTransform::IDENTITY,
            pressed_shift,
        );
        let focus_damage_outset = (self
            .style
            .length("--focus-ring-offset")
            .expect("typed focus offset")
            .pixels
            + self
                .style
                .length("--focus-ring-width")
                .expect("typed focus width")
                .pixels)
            * metrics.scale;
        let damage = damage(
            &self.previous,
            &current,
            metrics.scale,
            width,
            height,
            focus_damage_outset,
            self.theme_base,
        );
        self.previous = current;
        Ok(RasterFrame {
            width,
            height,
            rgba: pixmap.data().to_vec(),
            damage,
            notes,
        })
    }
}

impl Default for Rasterizer {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RenderError {
    InvalidSurface,
    UnknownStyleKey(String),
    InvalidTextScale,
}

fn physical(logical: f32, scale: f32) -> Result<u32, RenderError> {
    let value = logical * scale;
    if !value.is_finite() || value <= 0.0 || value > u32::MAX as f32 {
        Err(RenderError::InvalidSurface)
    } else {
        Ok(value.round() as u32)
    }
}

fn collect(
    node: &Node,
    parent_id: Option<&str>,
    sibling_index: usize,
    out: &mut Vec<NodeSnapshot>,
    ancestor_transform: LogicalTransform,
    pressed_shift: f32,
) {
    let transform = node_transform(node, ancestor_transform, pressed_shift);
    out.push(NodeSnapshot {
        id: node.id.as_str().into(),
        parent_id: parent_id.map(str::to_owned),
        sibling_index,
        bounds: transform.map_bounds(node.bounds),
        label: if paints_accessible_label(node) {
            node.accessible_label.clone()
        } else {
            String::new()
        },
        style_token: node.style_token.clone(),
        type_role: node.type_role,
        line_height: node.line_height,
        corner_radius: normalized_corner_radius(node.corner_radius),
        focused: node.state.focused,
        pressed: node.state.pressed,
        disabled: node.state.disabled,
        selected: node.state.selected,
        unavailable: node.state.unavailable,
        destructive: node.state.destructive,
        scrimmed: node.state.scrimmed,
        elevation: node.elevation,
        content: match &node.content {
            NodeContent::Label => ContentSnapshot::Label,
            NodeContent::Image { source, fit } => ContentSnapshot::Image {
                source_id: source.id.clone(),
                fit: *fit,
            },
        },
    });
    for (index, child) in node.children.iter().enumerate() {
        collect(
            child,
            Some(node.id.as_str()),
            index,
            out,
            transform,
            pressed_shift,
        );
    }
}

fn paints_accessible_label(node: &Node) -> bool {
    matches!(node.role, Role::Text | Role::Heading) && matches!(node.content, NodeContent::Label)
}

/// Axis-aligned logical-space transform carried by the paint walk. Pressed geometry only
/// scales and translates, so retaining this narrower representation avoids introducing
/// rotation/skew rounding into text layout and clipping.
#[derive(Clone, Copy)]
struct LogicalTransform {
    scale_x: f32,
    scale_y: f32,
    translate_x: f32,
    translate_y: f32,
}

impl LogicalTransform {
    const IDENTITY: Self = Self {
        scale_x: 1.0,
        scale_y: 1.0,
        translate_x: 0.0,
        translate_y: 0.0,
    };

    fn then(self, local: Self) -> Self {
        Self {
            scale_x: self.scale_x * local.scale_x,
            scale_y: self.scale_y * local.scale_y,
            translate_x: self.scale_x * local.translate_x + self.translate_x,
            translate_y: self.scale_y * local.translate_y + self.translate_y,
        }
    }

    fn map_bounds(self, bounds: Bounds) -> Bounds {
        Bounds::new(
            bounds.x * self.scale_x + self.translate_x,
            bounds.y * self.scale_y + self.translate_y,
            bounds.width * self.scale_x,
            bounds.height * self.scale_y,
        )
    }

    fn is_finite(self) -> bool {
        self.scale_x.is_finite()
            && self.scale_y.is_finite()
            && self.translate_x.is_finite()
            && self.translate_y.is_finite()
    }
}

fn node_transform(node: &Node, ancestor: LogicalTransform, pressed_shift: f32) -> LogicalTransform {
    if !node.state.pressed {
        return ancestor;
    }

    let pressed_axis = |origin: f32, size: f32| {
        if size == 0.0 {
            (1.0, 0.0)
        } else {
            let scale = (size - pressed_shift * 2.0).max(1.0) / size;
            (scale, origin + pressed_shift - origin * scale)
        }
    };
    let (scale_x, translate_x) = pressed_axis(node.bounds.x, node.bounds.width);
    let (scale_y, translate_y) = pressed_axis(node.bounds.y, node.bounds.height);
    let composed = ancestor.then(LogicalTransform {
        scale_x,
        scale_y,
        translate_x,
        translate_y,
    });
    debug_assert!(composed.is_finite(), "pressed transform must remain finite");
    composed
}

fn damage(
    old: &[NodeSnapshot],
    new: &[NodeSnapshot],
    scale: f32,
    width: u32,
    height: u32,
    focus_outset: f32,
    base: ThemeBase,
) -> Option<DamageRect> {
    if old.is_empty() {
        return Some(DamageRect {
            x: 0,
            y: 0,
            width,
            height,
        });
    }
    let mut result = None;
    for node in old.iter().chain(new) {
        let peer = if old.iter().any(|v| std::ptr::eq(v, node)) {
            new.iter().find(|v| v.id == node.id)
        } else {
            old.iter().find(|v| v.id == node.id)
        };
        if peer != Some(node) {
            let elevation_outset = effect_outset(base, node.elevation) * scale;
            let aura_outset = if node.focused {
                effect_outset(base, Elevation::Focus) * scale
            } else {
                0.0
            };
            let outset = elevation_outset.max(aura_outset).max(if node.focused {
                focus_outset
            } else {
                0.0
            });
            let r = bounds_rect(node.bounds, scale, width, height, outset);
            result = Some(result.map_or(r, |d: DamageRect| d.union(r)));
        }
    }
    result
}

fn shadow_asset(base: ThemeBase, elevation: Elevation) -> &'static ShadowAsset {
    SHADOW_ASSETS
        .iter()
        .find(|asset| asset.base == base && asset.elevation == elevation)
        .expect("build script emits every base/elevation asset")
}

/// Returns the deterministic build-generated RGBA 9-slice source for an elevation.
/// The bytes are immutable and may be used by conformance tests or alternate frame hosts.
pub fn prebaked_elevation_bytes(base: ThemeBase, elevation: Elevation) -> &'static [u8] {
    if elevation == Elevation::None {
        &[]
    } else {
        shadow_asset(base, elevation).rgba
    }
}

fn effect_outset(base: ThemeBase, elevation: Elevation) -> f32 {
    if elevation == Elevation::None {
        0.0
    } else {
        shadow_asset(base, elevation).margin as f32
    }
}

fn bounds_rect(b: Bounds, scale: f32, width: u32, height: u32, outset: f32) -> DamageRect {
    let x = (b.x * scale - outset).floor().max(0.0) as u32;
    let y = (b.y * scale - outset).floor().max(0.0) as u32;
    let right = ((b.x + b.width) * scale + outset).ceil().max(0.0) as u32;
    let bottom = ((b.y + b.height) * scale + outset).ceil().max(0.0) as u32;
    DamageRect {
        x: x.min(width),
        y: y.min(height),
        width: right.min(width).saturating_sub(x.min(width)),
        height: bottom.min(height).saturating_sub(y.min(height)),
    }
}

struct DrawContext<'a> {
    fonts: &'a mut tracked_text::FontSystem,
    glyphs: &'a mut tracked_text::SwashCache,
    images: &'a mut ImageCache,
    rounded_shadows: &'a mut RoundedShadowCache,
    notes: &'a mut Vec<RenderNote>,
    style: &'a ResolvedStyleSnapshot,
    typography: &'a TypographySnapshot,
    text_scale: f32,
}

fn style_color(style: &ResolvedStyleSnapshot, key: &str) -> Result<Rgba, RenderError> {
    style.color(key).map_err(|_| {
        debug_assert!(false, "unknown or non-color style key: {key}");
        RenderError::UnknownStyleKey(key.into())
    })
}

fn draw_node(
    pm: &mut Pixmap,
    context: &mut DrawContext<'_>,
    node: &Node,
    scale: f32,
    ancestor_transform: LogicalTransform,
) -> Result<(), RenderError> {
    let pressed_shift = context
        .style
        .length("--state-pressed-shift")
        .expect("typed pressed shift")
        .pixels;
    let transform = node_transform(node, ancestor_transform, pressed_shift);
    let b = transform.map_bounds(node.bounds);
    let corner_radius = normalized_corner_radius(node.corner_radius);
    let radius = clamped_radii(
        Radii::new(
            corner_radius * transform.scale_x.abs(),
            corner_radius * transform.scale_y.abs(),
        ),
        b,
    )
    .scaled(scale);

    if node.elevation != Elevation::None {
        draw_node_shadow(
            pm,
            context.rounded_shadows,
            shadow_asset(context.style.base(), node.elevation),
            b,
            scale,
            radius,
        );
    }
    if node.state.focused && node.elevation != Elevation::Focus {
        draw_node_shadow(
            pm,
            context.rounded_shadows,
            shadow_asset(context.style.base(), Elevation::Focus),
            b,
            scale,
            radius,
        );
    }

    let color = style_color(context.style, &node.style_token)?;
    if let Some(rect) = Rect::from_xywh(b.x * scale, b.y * scale, b.width * scale, b.height * scale)
    {
        let mut paint = Paint::default();
        paint.set_color_rgba8(color.red, color.green, color.blue, color.alpha);
        if radius.is_rounded() {
            pm.fill_path(
                &rounded_rect_path(rect, radius),
                &paint,
                tiny_skia::FillRule::Winding,
                Transform::identity(),
                None,
            );
        } else {
            pm.fill_rect(rect, &paint, Transform::identity(), None);
        }
        if node.state.selected {
            fill_token_rect_clipped(
                pm,
                context.style,
                "--state-selected-accent",
                Rect::from_xywh(rect.x(), rect.y(), 3.0 * scale, rect.height())
                    .expect("positive selected accent bounds"),
                rect,
                radius,
            )?;
        }
        if node.state.destructive {
            fill_token_rect_clipped(
                pm,
                context.style,
                "--state-destructive-accent",
                Rect::from_xywh(rect.x(), rect.y(), rect.width(), 3.0 * scale)
                    .expect("positive destructive accent bounds"),
                rect,
                radius,
            )?;
        }
    }
    let text_key = if node.state.disabled {
        "--state-disabled-text"
    } else if node.state.unavailable {
        "--state-unavailable-text"
    } else if node.state.focused {
        "--state-focused-text"
    } else {
        "--state-rest-text"
    };
    let text = style_color(context.style, text_key)?;
    match &node.content {
        NodeContent::Label if paints_accessible_label(node) => {
            let draw = TextDraw {
                text: &node.accessible_label,
                x: (b.x + 6.0) * scale,
                y: (b.y + 5.0) * scale,
                width: (b.width - 12.0).max(1.0) * scale,
                height: (b.height - 5.0).max(0.0) * scale,
                style: context.typography.resolve(node.type_role),
                text_scale: context.text_scale,
                surface_scale: scale,
                line_height: node.line_height,
                clip: (b.x * scale, b.y * scale, b.width * scale, b.height * scale),
                color: text,
            };
            draw_text(pm, context.fonts, context.glyphs, draw);
        }
        NodeContent::Label => {}
        NodeContent::Image { source, fit } => {
            if let Err(note) = draw_image(pm, context.images, source, *fit, b, scale, radius) {
                context.notes.push(note);
            }
        }
    }
    for child in &node.children {
        draw_node(pm, context, child, scale, transform)?;
    }
    if node.state.disabled {
        if let Some(rect) =
            Rect::from_xywh(b.x * scale, b.y * scale, b.width * scale, b.height * scale)
        {
            stroke_token_rect(
                pm,
                context.style,
                "--state-disabled-border",
                rect,
                scale,
                radius,
            )?;
        }
        draw_state_glyph(pm, context.style, b, scale, StateGlyph::Disabled)?;
    }
    if node.state.unavailable {
        fill_token_rounded_rect(
            pm,
            context.style,
            "--state-unavailable-veil",
            b,
            scale,
            radius,
        )?;
        draw_state_glyph(pm, context.style, b, scale, StateGlyph::Unavailable)?;
    }
    if node.state.destructive {
        draw_state_glyph(pm, context.style, b, scale, StateGlyph::Destructive)?;
    }
    if node.state.scrimmed {
        fill_token_rounded_rect(pm, context.style, "--color-surface-scrim", b, scale, radius)?;
    }
    if node.state.focused {
        draw_focus_ring(pm, context.style, b, scale, radius)?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum StateGlyph {
    Disabled,
    Unavailable,
    Destructive,
}

/// Draws the renderer-owned, non-color half of the state cue grammar. Keeping these
/// marks outside node content prevents a component from accidentally omitting them.
fn draw_state_glyph(
    pm: &mut Pixmap,
    style: &ResolvedStyleSnapshot,
    b: Bounds,
    scale: f32,
    glyph: StateGlyph,
) -> Result<(), RenderError> {
    let size = b.width.min(b.height).min(14.0);
    let left = (b.x + b.width - size - 5.0) * scale;
    let top = (b.y + 5.0) * scale;
    let size = size * scale;
    let key = match glyph {
        StateGlyph::Disabled => "--state-disabled-text",
        StateGlyph::Unavailable => "--state-unavailable-text",
        StateGlyph::Destructive => "--state-destructive-accent",
    };
    let color = style_color(style, key)?;
    let mut paint = Paint::default();
    paint.set_color_rgba8(color.red, color.green, color.blue, color.alpha);
    let stroke = Stroke {
        width: (2.0 * scale).max(1.0),
        ..Stroke::default()
    };
    let mut path = PathBuilder::new();
    match glyph {
        StateGlyph::Disabled => {
            path.move_to(left + size * 0.12, top + size * 0.5);
            path.line_to(left + size * 0.88, top + size * 0.5);
        }
        StateGlyph::Unavailable => {
            path.move_to(left + size * 0.12, top + size * 0.88);
            path.line_to(left + size * 0.88, top + size * 0.12);
        }
        StateGlyph::Destructive => {
            path.move_to(left + size * 0.5, top + size * 0.08);
            path.line_to(left + size * 0.94, top + size * 0.9);
            path.line_to(left + size * 0.06, top + size * 0.9);
            path.close();
            path.move_to(left + size * 0.5, top + size * 0.3);
            path.line_to(left + size * 0.5, top + size * 0.62);
            path.move_to(left + size * 0.5, top + size * 0.76);
            path.line_to(left + size * 0.5, top + size * 0.78);
        }
    }
    if let Some(path) = path.finish() {
        pm.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
    Ok(())
}

fn fill_token_rect_clipped(
    pm: &mut Pixmap,
    style: &ResolvedStyleSnapshot,
    key: &str,
    fill_rect: Rect,
    silhouette: Rect,
    radius: Radii,
) -> Result<(), RenderError> {
    let color = style_color(style, key)?;
    let mut paint = Paint::default();
    paint.set_color_rgba8(color.red, color.green, color.blue, color.alpha);
    let mut mask = Mask::new(pm.width(), pm.height()).expect("pixmap has valid dimensions");
    mask.fill_path(
        &if radius.is_rounded() {
            rounded_rect_path(silhouette, radius)
        } else {
            PathBuilder::from_rect(silhouette)
        },
        tiny_skia::FillRule::Winding,
        true,
        Transform::identity(),
    );
    pm.fill_rect(fill_rect, &paint, Transform::identity(), Some(&mask));
    Ok(())
}

fn fill_token_rounded_rect(
    pm: &mut Pixmap,
    style: &ResolvedStyleSnapshot,
    key: &str,
    bounds: Bounds,
    scale: f32,
    radius: Radii,
) -> Result<(), RenderError> {
    let color = style_color(style, key)?;
    if let Some(rect) = Rect::from_xywh(
        bounds.x * scale,
        bounds.y * scale,
        bounds.width * scale,
        bounds.height * scale,
    ) {
        let mut paint = Paint::default();
        paint.set_color_rgba8(color.red, color.green, color.blue, color.alpha);
        if radius.is_rounded() {
            pm.fill_path(
                &rounded_rect_path(rect, radius),
                &paint,
                tiny_skia::FillRule::Winding,
                Transform::identity(),
                None,
            );
        } else {
            pm.fill_rect(rect, &paint, Transform::identity(), None);
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Radii {
    x: f32,
    y: f32,
}

impl Radii {
    const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    fn scaled(self, scale: f32) -> Self {
        Self::new(self.x * scale, self.y * scale)
    }

    fn is_rounded(self) -> bool {
        self.x > 0.0 && self.y > 0.0
    }
}

fn clamped_radii(radius: Radii, bounds: Bounds) -> Radii {
    let clamp = |value: f32, extent: f32| {
        if value.is_finite() && value > 0.0 {
            value.min(extent.max(0.0) / 2.0)
        } else {
            0.0
        }
    };
    Radii::new(
        clamp(radius.x, bounds.width),
        clamp(radius.y, bounds.height),
    )
}

fn normalized_corner_radius(radius: f32) -> f32 {
    if radius.is_finite() && radius > 0.0 {
        radius
    } else {
        0.0
    }
}

fn rounded_rect_path(rect: Rect, radius: Radii) -> tiny_skia::Path {
    let rx = radius.x.min(rect.width() / 2.0).max(0.0);
    let ry = radius.y.min(rect.height() / 2.0).max(0.0);
    if rx == 0.0 || ry == 0.0 {
        return PathBuilder::from_rect(rect);
    }
    // Standard cubic approximation of a quarter circle.
    const KAPPA: f32 = 0.552_284_8;
    let left = rect.left();
    let top = rect.top();
    let right = rect.right();
    let bottom = rect.bottom();
    let control_x = rx * KAPPA;
    let control_y = ry * KAPPA;
    let mut path = PathBuilder::new();
    path.move_to(left + rx, top);
    path.line_to(right - rx, top);
    path.cubic_to(
        right - rx + control_x,
        top,
        right,
        top + ry - control_y,
        right,
        top + ry,
    );
    path.line_to(right, bottom - ry);
    path.cubic_to(
        right,
        bottom - ry + control_y,
        right - rx + control_x,
        bottom,
        right - rx,
        bottom,
    );
    path.line_to(left + rx, bottom);
    path.cubic_to(
        left + rx - control_x,
        bottom,
        left,
        bottom - ry + control_y,
        left,
        bottom - ry,
    );
    path.line_to(left, top + ry);
    path.cubic_to(
        left,
        top + ry - control_y,
        left + rx - control_x,
        top,
        left + rx,
        top,
    );
    path.close();
    path.finish().expect("rounded rectangle path")
}

fn rounded_coverage(x: f32, y: f32, width: f32, height: f32, radius: Radii) -> f32 {
    let rx = radius.x.min(width / 2.0).max(0.0);
    let ry = radius.y.min(height / 2.0).max(0.0);
    if rx == 0.0 || ry == 0.0 {
        return 1.0;
    }
    let cx = x.clamp(rx, width - rx);
    let cy = y.clamp(ry, height - ry);
    if rx == ry {
        let distance = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
        return (rx + 0.5 - distance).clamp(0.0, 1.0);
    }
    let distance = (((x - cx) / rx).powi(2) + ((y - cy) / ry).powi(2)).sqrt();
    let aa = 0.5 / rx.min(ry);
    ((1.0 + aa - distance) / (aa * 2.0)).clamp(0.0, 1.0)
}

fn stroke_token_rect(
    pm: &mut Pixmap,
    style: &ResolvedStyleSnapshot,
    key: &str,
    rect: Rect,
    scale: f32,
    radius: Radii,
) -> Result<(), RenderError> {
    let color = style_color(style, key)?;
    let mut paint = Paint::default();
    paint.set_color_rgba8(color.red, color.green, color.blue, color.alpha);
    pm.stroke_path(
        &if radius.is_rounded() {
            rounded_rect_path(rect, radius)
        } else {
            PathBuilder::from_rect(rect)
        },
        &paint,
        &Stroke {
            width: scale,
            dash: StrokeDash::new(vec![4.0 * scale, 3.0 * scale], 0.0),
            ..Stroke::default()
        },
        Transform::identity(),
        None,
    );
    Ok(())
}

fn draw_focus_ring(
    pm: &mut Pixmap,
    style: &ResolvedStyleSnapshot,
    b: Bounds,
    scale: f32,
    radius: Radii,
) -> Result<(), RenderError> {
    let width = style
        .length("--focus-ring-width")
        .expect("typed focus width")
        .pixels
        * scale;
    let offset = style
        .length("--focus-ring-offset")
        .expect("typed focus offset")
        .pixels
        * scale;
    let outset = offset + width / 2.0;
    if let Some(rect) = Rect::from_xywh(
        b.x * scale - outset,
        b.y * scale - outset,
        b.width * scale + outset * 2.0,
        b.height * scale + outset * 2.0,
    ) {
        let ring = style_color(style, "--state-focused-ring")?;
        let mut paint = Paint::default();
        paint.set_color_rgba8(ring.red, ring.green, ring.blue, ring.alpha);
        pm.stroke_path(
            &if radius.is_rounded() {
                rounded_rect_path(rect, Radii::new(radius.x + outset, radius.y + outset))
            } else {
                PathBuilder::from_rect(rect)
            },
            &paint,
            &Stroke {
                width,
                ..Stroke::default()
            },
            Transform::identity(),
            None,
        );
    }
    Ok(())
}

fn draw_node_shadow(
    pm: &mut Pixmap,
    cache: &mut RoundedShadowCache,
    asset: &ShadowAsset,
    b: Bounds,
    scale: f32,
    radius: Radii,
) {
    if asset.margin == 0 || asset.color[3] == 0 {
        return;
    }
    if !radius.is_rounded() {
        draw_shadow(
            pm,
            asset.rgba,
            asset.side,
            asset.margin,
            asset.margin,
            b,
            scale,
        );
        return;
    }
    let physical_radius = quantized_physical_shadow_radii(b, scale, radius);
    let key = (
        theme_base_key(asset.base),
        elevation_key(asset.elevation),
        physical_radius.0,
        physical_radius.1,
        ((asset.margin as f32 * scale).round().clamp(1.0, 64.0)) as u16,
    );
    let rounded = cache.get_or_bake(key, asset);
    let destination_margin_x = physical_radius.0 + u32::from(key.4);
    let destination_margin_y = physical_radius.1 + u32::from(key.4);
    draw_shadow_with_destination_margins(
        pm,
        &rounded.rgba,
        rounded.width,
        rounded.height,
        rounded.slice_margin_x,
        rounded.slice_margin_y,
        rounded.effect_margin,
        b,
        scale,
        destination_margin_x as i32,
        destination_margin_y as i32,
    );
}

fn quantized_physical_shadow_radii(b: Bounds, scale: f32, radius: Radii) -> (u32, u32) {
    let quantize = |value: f32, extent: f32| {
        value.round().clamp(
            1.0,
            ((extent * scale).abs() * 0.5)
                .floor()
                .max(1.0)
                .min(MAX_ROUNDED_SHADOW_RADIUS as f32),
        ) as u32
    };
    (quantize(radius.x, b.width), quantize(radius.y, b.height))
}

fn theme_base_key(base: ThemeBase) -> u8 {
    match base {
        ThemeBase::Dusk => 0,
        ThemeBase::Day => 1,
        ThemeBase::HighContrast => 2,
    }
}

fn elevation_key(elevation: Elevation) -> u8 {
    match elevation {
        Elevation::None => 0,
        Elevation::Elev1 => 1,
        Elevation::Elev2 => 2,
        Elevation::Focus => 3,
    }
}

#[cfg(test)]
fn bake_rounded_shadow(asset: &ShadowAsset, radius: usize) -> RoundedShadowAsset {
    bake_rounded_shadow_physical(
        asset,
        Radii::new(radius as f32, radius as f32),
        asset.margin,
    )
}

fn bake_rounded_shadow_physical(
    asset: &ShadowAsset,
    radius: Radii,
    physical_margin: usize,
) -> RoundedShadowAsset {
    let effect_scale = physical_margin as f32 / asset.margin as f32;
    let radius_x = radius.x as usize;
    let radius_y = radius.y as usize;
    let core_width = radius_x * 2 + 3;
    let core_height = radius_y * 2 + 3;
    let width_px = physical_margin * 2 + core_width;
    let height_px = physical_margin * 2 + core_height;
    let mut mask = vec![0.0f32; width_px * height_px];
    let grow = (asset.spread * effect_scale).round() as isize;
    let left = physical_margin as isize + (asset.offset_x * effect_scale).round() as isize - grow;
    let top = physical_margin as isize + (asset.offset_y * effect_scale).round() as isize - grow;
    let silhouette_width = core_width as f32 + (grow * 2) as f32;
    let silhouette_height = core_height as f32 + (grow * 2) as f32;
    let expanded_radius = Radii::new(
        (radius.x + grow as f32).max(0.0),
        (radius.y + grow as f32).max(0.0),
    );
    for y in 0..height_px {
        for x in 0..width_px {
            mask[y * width_px + x] = rounded_coverage(
                x as f32 + 0.5 - left as f32,
                y as f32 + 0.5 - top as f32,
                silhouette_width,
                silhouette_height,
                expanded_radius,
            );
        }
    }
    blur_mask(&mut mask, width_px, height_px, asset.blur * effect_scale);
    // The legacy 3px source establishes the shipped straight-edge penumbra intensity. A larger
    // rounded source has more blur mass, so normalize it to that edge sample while retaining the
    // rounded mask's corner falloff.
    let legacy_edge_alpha =
        f32::from(asset.rgba[(asset.margin * asset.side + asset.side / 2) * 4 + 3]);
    let rounded_edge_alpha =
        mask[physical_margin * width_px + width_px / 2] * f32::from(asset.color[3]);
    let normalization = if rounded_edge_alpha > 0.0 {
        legacy_edge_alpha / rounded_edge_alpha
    } else {
        1.0
    };
    let mut rgba = Vec::with_capacity(width_px * height_px * 4);
    for alpha in mask {
        rgba.extend_from_slice(&[
            asset.color[0],
            asset.color[1],
            asset.color[2],
            (alpha * normalization * f32::from(asset.color[3])).round() as u8,
        ]);
    }
    RoundedShadowAsset {
        width: width_px,
        height: height_px,
        slice_margin_x: physical_margin + radius_x,
        slice_margin_y: physical_margin + radius_y,
        // draw_shadow scales this destination-only value; source slicing uses slice_margin above.
        effect_margin: asset.margin,
        rgba,
    }
}

fn blur_mask(mask: &mut [f32], width: usize, height: usize, blur: f32) {
    if blur <= 0.0 {
        return;
    }
    let radius = blur.ceil() as isize;
    let sigma = blur / 2.0;
    let kernel: Vec<f32> = (-radius..=radius)
        .map(|x| (-(x * x) as f32 / (2.0 * sigma * sigma)).exp())
        .collect();
    let sum: f32 = kernel.iter().sum();
    let kernel: Vec<f32> = kernel.into_iter().map(|value| value / sum).collect();
    let mut tmp = vec![0.0; mask.len()];
    for y in 0..height {
        for x in 0..width {
            tmp[y * width + x] = (-radius..=radius)
                .filter_map(|offset| {
                    usize::try_from(x as isize + offset)
                        .ok()
                        .filter(|position| *position < width)
                        .map(|position| {
                            mask[y * width + position] * kernel[(offset + radius) as usize]
                        })
                })
                .sum();
        }
    }
    for y in 0..height {
        for x in 0..width {
            mask[y * width + x] = (-radius..=radius)
                .filter_map(|offset| {
                    usize::try_from(y as isize + offset)
                        .ok()
                        .filter(|position| *position < height)
                        .map(|position| {
                            tmp[position * width + x] * kernel[(offset + radius) as usize]
                        })
                })
                .sum();
        }
    }
}

fn draw_shadow(
    pm: &mut Pixmap,
    rgba: &[u8],
    side: usize,
    source_margin: usize,
    effect_margin: usize,
    b: Bounds,
    scale: f32,
) {
    let destination_margin = (source_margin as f32 * scale).round().max(1.0) as i32;
    draw_shadow_with_destination_margin(
        pm,
        rgba,
        side,
        source_margin,
        effect_margin,
        b,
        scale,
        destination_margin,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_shadow_with_destination_margin(
    pm: &mut Pixmap,
    rgba: &[u8],
    side: usize,
    source_margin: usize,
    effect_margin: usize,
    b: Bounds,
    scale: f32,
    destination_margin: i32,
) {
    draw_shadow_with_destination_margins(
        pm,
        rgba,
        side,
        side,
        source_margin,
        source_margin,
        effect_margin,
        b,
        scale,
        destination_margin,
        destination_margin,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_shadow_with_destination_margins(
    pm: &mut Pixmap,
    rgba: &[u8],
    source_width: usize,
    source_height: usize,
    source_margin_x: usize,
    source_margin_y: usize,
    effect_margin: usize,
    b: Bounds,
    scale: f32,
    destination_margin_x: i32,
    destination_margin_y: i32,
) {
    if effect_margin == 0 {
        return;
    }
    let margin = (effect_margin as f32 * scale).round().max(1.0) as i32;
    let left = (b.x * scale).round() as i32 - margin;
    let top = (b.y * scale).round() as i32 - margin;
    let width = (b.width * scale).round().max(1.0) as i32 + margin * 2;
    let height = (b.height * scale).round().max(1.0) as i32 + margin * 2;
    for dy in 0..height {
        let sy = slice_coordinate(
            dy,
            height,
            source_height,
            source_margin_y,
            destination_margin_y,
        );
        for dx in 0..width {
            let x = left + dx;
            let y = top + dy;
            if x < 0 || y < 0 || x >= pm.width() as i32 || y >= pm.height() as i32 {
                continue;
            }
            let sx = slice_coordinate(
                dx,
                width,
                source_width,
                source_margin_x,
                destination_margin_x,
            );
            let index = (sy * source_width + sx) * 4;
            let color: [u8; 4] = rgba[index..index + 4].try_into().unwrap();
            blend_pixel(pm, x as u32, y as u32, color);
        }
    }
}

fn slice_coordinate(
    position: i32,
    destination: i32,
    side: usize,
    source_margin: usize,
    destination_margin: i32,
) -> usize {
    if position < destination_margin {
        (position.max(0) as usize * source_margin / destination_margin as usize)
            .min(source_margin.saturating_sub(1))
    } else if position >= destination - destination_margin {
        let distance = (destination - 1 - position).max(0) as usize;
        side - 1
            - (distance * source_margin / destination_margin as usize)
                .min(source_margin.saturating_sub(1))
    } else {
        source_margin + 1
    }
}

fn blend_pixel(pm: &mut Pixmap, x: u32, y: u32, color: [u8; 4]) {
    let alpha = u32::from(color[3]);
    if alpha == 0 {
        return;
    }
    let width = pm.width() as usize;
    let dst = &mut pm.pixels_mut()[y as usize * width + x as usize];
    let old = dst.demultiply();
    let inv = 255 - alpha;
    *dst = tiny_skia::PremultipliedColorU8::from_rgba(
        ((u32::from(color[0]) * alpha + u32::from(old.red()) * inv) / 255) as u8,
        ((u32::from(color[1]) * alpha + u32::from(old.green()) * inv) / 255) as u8,
        ((u32::from(color[2]) * alpha + u32::from(old.blue()) * inv) / 255) as u8,
        255,
    )
    .expect("opaque blend");
}

fn draw_image(
    target: &mut Pixmap,
    cache: &mut ImageCache,
    source: &ImageSource,
    fit: ImageFit,
    bounds: Bounds,
    surface_scale: f32,
    radius: Radii,
) -> Result<(), RenderNote> {
    let image = cache.get_or_decode(source)?;
    let target_width = bounds.width * surface_scale;
    let target_height = bounds.height * surface_scale;
    if target_width <= 0.0 || target_height <= 0.0 {
        return Ok(());
    }
    let scale_x = target_width / image.width() as f32;
    let scale_y = target_height / image.height() as f32;
    let image_scale = match fit {
        ImageFit::Cover => scale_x.max(scale_y),
        ImageFit::Contain => scale_x.min(scale_y),
    };
    let draw_width = image.width() as f32 * image_scale;
    let draw_height = image.height() as f32 * image_scale;
    let x = bounds.x * surface_scale + (target_width - draw_width) / 2.0;
    let y = bounds.y * surface_scale + (target_height - draw_height) / 2.0;

    let mut mask_data = vec![0; target.width() as usize * target.height() as usize];
    let left = (bounds.x * surface_scale).floor().max(0.0) as u32;
    let top = (bounds.y * surface_scale).floor().max(0.0) as u32;
    let right = ((bounds.x + bounds.width) * surface_scale)
        .ceil()
        .clamp(0.0, target.width() as f32) as u32;
    let bottom = ((bounds.y + bounds.height) * surface_scale)
        .ceil()
        .clamp(0.0, target.height() as f32) as u32;
    if radius.is_rounded() {
        let rect = Rect::from_xywh(
            bounds.x * surface_scale,
            bounds.y * surface_scale,
            target_width,
            target_height,
        )
        .expect("positive image bounds");
        let mut mask = Mask::from_vec(
            mask_data,
            tiny_skia::IntSize::from_wh(target.width(), target.height())
                .expect("pixmap has valid dimensions"),
        )
        .expect("mask matches target dimensions");
        mask.fill_path(
            &rounded_rect_path(rect, radius),
            tiny_skia::FillRule::Winding,
            true,
            Transform::identity(),
        );
        mask_data = mask.data().to_vec();
    } else {
        for row in top.min(target.height())..bottom {
            let start = (row * target.width() + left.min(target.width())) as usize;
            let end = (row * target.width() + right) as usize;
            mask_data[start..end].fill(255);
        }
    }
    let mask_size = tiny_skia::IntSize::from_wh(target.width(), target.height())
        .expect("pixmap has valid dimensions");
    let mask = Mask::from_vec(mask_data, mask_size).expect("mask matches target dimensions");
    let paint = PixmapPaint {
        quality: FilterQuality::Bilinear,
        ..PixmapPaint::default()
    };
    let transform = Transform::from_scale(image_scale, image_scale).post_translate(x, y);
    target.draw_pixmap(0, 0, image.as_ref(), &paint, transform, Some(&mask));
    Ok(())
}

impl ImageCache {
    fn get_or_decode(&mut self, source: &ImageSource) -> Result<&Pixmap, RenderNote> {
        if !self.decoded.contains_key(&source.id) {
            let pixmap = decode_png(source)?;
            if self.decoded.len() == IMAGE_CACHE_CAPACITY {
                if let Some(evicted) = self.insertion_order.pop_front() {
                    self.decoded.remove(&evicted);
                }
            }
            self.insertion_order.push_back(source.id.clone());
            self.decoded.insert(source.id.clone(), pixmap);
        }
        Ok(self
            .decoded
            .get(&source.id)
            .expect("just inserted or present"))
    }
}

fn decode_png(source: &ImageSource) -> Result<Pixmap, RenderNote> {
    let failed = || RenderNote::ImageDecodeFailed {
        source_id: source.id.clone(),
    };
    let bytes = source.bytes.as_ref();
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" || &bytes[12..16] != b"IHDR" {
        return Err(failed());
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().expect("four-byte width"));
    let height = u32::from_be_bytes(bytes[20..24].try_into().expect("four-byte height"));
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_IMAGE_PIXELS {
        return Err(RenderNote::ImageTooLarge {
            source_id: source.id.clone(),
            width,
            height,
            max_pixels: MAX_IMAGE_PIXELS,
        });
    }
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    decoder.set_limits(png::Limits {
        bytes: MAX_IMAGE_PIXELS as usize * 4,
    });
    let mut reader = decoder.read_info().map_err(|_| failed())?;
    let info = reader.info();
    debug_assert_eq!((info.width, info.height), (width, height));
    let mut decoded = vec![0; reader.output_buffer_size()];
    let output = reader.next_frame(&mut decoded).map_err(|_| failed())?;
    let bytes = &decoded[..output.buffer_size()];
    let mut rgba = Vec::with_capacity(pixels as usize * 4);
    match output.color_type {
        png::ColorType::Rgba => rgba.extend_from_slice(bytes),
        png::ColorType::Rgb => bytes
            .chunks_exact(3)
            .for_each(|p| rgba.extend_from_slice(&[p[0], p[1], p[2], 255])),
        png::ColorType::Grayscale => bytes
            .iter()
            .for_each(|&v| rgba.extend_from_slice(&[v, v, v, 255])),
        png::ColorType::GrayscaleAlpha => bytes
            .chunks_exact(2)
            .for_each(|p| rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]])),
        png::ColorType::Indexed => return Err(failed()),
    }
    let mut pixmap = Pixmap::new(output.width, output.height).ok_or_else(failed)?;
    for (source, target) in rgba.chunks_exact(4).zip(pixmap.pixels_mut()) {
        *target =
            tiny_skia::ColorU8::from_rgba(source[0], source[1], source[2], source[3]).premultiply();
    }
    Ok(pixmap)
}

struct TextDraw<'a> {
    text: &'a str,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    style: &'a ResolvedTypeStyle,
    text_scale: f32,
    surface_scale: f32,
    line_height: Option<f32>,
    clip: (f32, f32, f32, f32),
    color: Rgba,
}

fn draw_text(
    pm: &mut Pixmap,
    fonts: &mut tracked_text::FontSystem,
    glyphs: &mut tracked_text::SwashCache,
    draw: TextDraw<'_>,
) {
    let size = draw.style.size_px * draw.text_scale * draw.surface_scale;
    let line_height = draw.line_height.map_or(size * 1.25, |value| size * value);
    let mut buffer =
        tracked_text::Buffer::new(fonts, tracked_text::Metrics::new(size, line_height));
    buffer.set_size(fonts, Some(draw.width), Some(draw.height));
    buffer.set_text(
        fonts,
        draw.text,
        &tracked_text::Attrs::new()
            .family(tracked_text::Family::Name(&draw.style.family))
            .weight(tracked_text::Weight(draw.style.weight))
            // Cosmic Text's public letter-spacing unit is em. It resolves the value
            // against Metrics::font_size while shaping, so converting it to pixels
            // here would multiply by the font size a second time.
            .letter_spacing(draw.style.tracking_em),
        tracked_text::Shaping::Advanced,
        None,
    );
    buffer.shape_until_scroll(fonts, false);
    let default_color = tracked_text::Color::rgba(
        draw.color.red,
        draw.color.green,
        draw.color.blue,
        draw.color.alpha,
    );
    for run in buffer.layout_runs() {
        for glyph in run.glyphs {
            let physical = glyph.physical((0.0, 0.0), 1.0);
            let color = glyph.color_opt.unwrap_or(default_color);
            glyphs.with_pixels(fonts, physical.cache_key, color, |x, y, color| {
                blend_text_pixel_rgba(
                    pm,
                    &draw,
                    physical.x + x,
                    run.line_y as i32 + physical.y + y,
                    [color.r(), color.g(), color.b(), color.a()],
                );
            });
        }
    }
}

fn blend_text_pixel_rgba(pm: &mut Pixmap, draw: &TextDraw<'_>, gx: i32, gy: i32, color: [u8; 4]) {
    let px = gx + draw.x as i32;
    let py = gy + draw.y as i32;
    if px < draw.clip.0.floor() as i32
        || py < draw.clip.1.floor() as i32
        || px >= (draw.clip.0 + draw.clip.2).ceil() as i32
        || py >= (draw.clip.1 + draw.clip.3).ceil() as i32
        || px < 0
        || py < 0
        || px >= pm.width() as i32
        || py >= pm.height() as i32
    {
        return;
    }
    let alpha = color[3] as u32;
    let pixmap_width = pm.width() as usize;
    let dst = &mut pm.pixels_mut()[py as usize * pixmap_width + px as usize];
    let old = dst.demultiply();
    let inv = 255 - alpha;
    *dst = tiny_skia::PremultipliedColorU8::from_rgba(
        ((color[0] as u32 * alpha + old.red() as u32 * inv) / 255) as u8,
        ((color[1] as u32 * alpha + old.green() as u32 * inv) / 255) as u8,
        ((color[2] as u32 * alpha + old.blue() as u32 * inv) / 255) as u8,
        255,
    )
    .expect("opaque text blend is valid");
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_scene::{ImageSource, Insets, NodeAction, NodeId, Orientation, Role};
    use std::sync::Arc;

    const IMAGE_PNG: &[u8] = include_bytes!("../../../spikes/consent-ui/baseline/s01-initial.png");
    const CORRUPT_PNG: &[u8] = include_bytes!("../tests/fixtures/corrupt.png");
    fn fixture(label: &str) -> Scene {
        let caption = Node::new(
            NodeId::new("caption").unwrap(),
            Role::Text,
            label,
            Bounds::new(3.0, 4.0, 250.0, 80.0),
            "--state-rest-surface",
        );
        let root = Node::new(
            NodeId::new("root").unwrap(),
            Role::Button,
            label,
            Bounds::new(3.0, 4.0, 250.0, 80.0),
            "--state-rest-surface",
        )
        .with_action(NodeAction::Activate)
        .with_children(vec![caption]);
        Scene::new(root, NodeId::new("root").unwrap()).unwrap()
    }
    fn metrics() -> SurfaceMetrics {
        SurfaceMetrics {
            logical_width: 431.0,
            logical_height: 277.0,
            scale: 1.0,
            safe_insets: Insets::default(),
            orientation: Orientation::Landscape,
        }
    }

    fn label_scene(role: Role, label: &str) -> Scene {
        let root = Node::new(
            NodeId::new("label").unwrap(),
            role,
            label,
            Bounds::new(20.0, 20.0, 160.0, 60.0),
            "--state-rest-surface",
        );
        Scene::new(root, NodeId::new("label").unwrap()).unwrap()
    }

    #[test]
    fn accessible_labels_paint_only_for_text_roles() {
        for role in [Role::Button, Role::ListItem, Role::Group, Role::Toggle] {
            let labeled = Rasterizer::new()
                .render(&label_scene(role, "container name"), metrics())
                .unwrap();
            let unlabeled = Rasterizer::new()
                .render(&label_scene(role, ""), metrics())
                .unwrap();
            assert_eq!(labeled.rgba, unlabeled.rgba, "{role:?} label painted");
        }

        for role in [Role::Text, Role::Heading] {
            let labeled = Rasterizer::new()
                .render(&label_scene(role, "visible text"), metrics())
                .unwrap();
            let unlabeled = Rasterizer::new()
                .render(&label_scene(role, ""), metrics())
                .unwrap();
            assert_ne!(labeled.rgba, unlabeled.rgba, "{role:?} label did not paint");
        }
    }

    #[test]
    fn non_text_accessible_label_changes_do_not_damage_rendered_content() {
        for role in [Role::Button, Role::ListItem, Role::Group, Role::Toggle] {
            let mut rasterizer = Rasterizer::new();
            let unnamed = rasterizer
                .render(&label_scene(role, ""), metrics())
                .unwrap();
            let named = rasterizer
                .render(&label_scene(role, "semantic name"), metrics())
                .unwrap();
            assert_eq!(named.damage, None, "{role:?} label caused damage");
            assert_eq!(named.rgba, unnamed.rgba, "{role:?} label changed pixels");
        }
    }

    #[test]
    fn only_painted_accessible_label_changes_damage_rendered_content() {
        let image_scene = |label| {
            let root = Node::new(
                NodeId::new("image").unwrap(),
                Role::Text,
                label,
                Bounds::new(20.0, 20.0, 160.0, 60.0),
                "--state-rest-surface",
            )
            .with_image(
                ImageSource::new("alt-text-image", Arc::<[u8]>::from(IMAGE_PNG)),
                ImageFit::Cover,
            );
            Scene::new(root, NodeId::new("image").unwrap()).unwrap()
        };

        let mut image_rasterizer = Rasterizer::new();
        let original_image = image_rasterizer
            .render(&image_scene("original alt text"), metrics())
            .unwrap();
        let renamed_image = image_rasterizer
            .render(&image_scene("updated alt text"), metrics())
            .unwrap();
        assert_eq!(renamed_image.damage, None);
        assert_eq!(renamed_image.rgba, original_image.rgba);

        let mut text_rasterizer = Rasterizer::new();
        text_rasterizer
            .render(&label_scene(Role::Text, "original text"), metrics())
            .unwrap();
        let renamed_text = text_rasterizer
            .render(&label_scene(Role::Text, "updated text"), metrics())
            .unwrap();
        assert!(renamed_text.damage.is_some());
    }

    #[test]
    fn deterministic_between_fresh_runs() {
        assert_eq!(
            Rasterizer::new()
                .render(&fixture("再開 日本語"), metrics())
                .unwrap()
                .rgba,
            Rasterizer::new()
                .render(&fixture("再開 日本語"), metrics())
                .unwrap()
                .rgba
        );
    }

    fn rounded_fixture(radius: f32) -> Scene {
        let root = Node::new(
            NodeId::new("rounded").unwrap(),
            Role::Group,
            "",
            Bounds::new(20.0, 20.0, 40.0, 40.0),
            "--state-rest-surface",
        )
        .with_corner_radius(radius);
        Scene::new(root, NodeId::new("rounded").unwrap()).unwrap()
    }

    #[test]
    fn rounded_fills_have_transparent_aa_corners_and_pill_clamp() {
        let background = token_rgba(ThemeBase::Dusk, "--color-surface-canvas");
        let fill = token_rgba(ThemeBase::Dusk, "--state-rest-surface");
        for radius in [6.0, 10.0, 16.0] {
            let frame = Rasterizer::new()
                .render(&rounded_fixture(radius), metrics())
                .unwrap();
            assert_eq!(pixel(&frame, 20, 20), background, "radius {radius}");
            assert_eq!(
                pixel(&frame, 20 + radius as u32, 21),
                fill,
                "radius {radius}"
            );
            assert!(
                (20..20 + radius as u32 + 1).any(|x| {
                    let p = pixel(&frame, x, 20);
                    p != background && p != fill
                }),
                "radius {radius} has no antialias coverage"
            );
        }
        let clamped = Rasterizer::new()
            .render(&rounded_fixture(20.0), metrics())
            .unwrap();
        let pill = Rasterizer::new()
            .render(&rounded_fixture(999.0), metrics())
            .unwrap();
        assert_eq!(clamped.rgba, pill.rgba);
    }

    #[test]
    fn non_uniform_pressed_rounding_uses_elliptical_fill_and_ring_geometry() {
        let mut node = Node::new(
            NodeId::new("ellipse").unwrap(),
            Role::Group,
            "",
            Bounds::new(20.0, 20.0, 100.0, 20.0),
            "--state-rest-surface",
        )
        .with_corner_radius(16.0);
        node.state.pressed = true;
        node.state.focused = true;
        let scene = Scene::new(node, NodeId::new("ellipse").unwrap()).unwrap();
        let frame = Rasterizer::new().render(&scene, metrics()).unwrap();
        let canvas = token_rgba(ThemeBase::Dusk, "--color-surface-canvas");
        let fill = token_rgba(ThemeBase::Dusk, "--state-rest-surface");

        // Press maps the 100x20 node to 98x18: rx=15.68 and ry clamps to 9. The top
        // edge therefore begins near x=37, not x=30 as the old min-scale circle did.
        assert_eq!(pixel(&frame, 25, 21), canvas);
        assert_eq!(pixel(&frame, 38, 21), fill);

        let style = Rasterizer::new().style;
        let bounds = Bounds::new(21.0, 21.0, 98.0, 18.0);
        let mut elliptical_ring = Pixmap::new(140, 60).unwrap();
        draw_focus_ring(
            &mut elliptical_ring,
            &style,
            bounds,
            1.0,
            Radii::new(15.68, 9.0),
        )
        .unwrap();
        let mut old_circular_ring = Pixmap::new(140, 60).unwrap();
        draw_focus_ring(
            &mut old_circular_ring,
            &style,
            bounds,
            1.0,
            Radii::new(9.0, 9.0),
        )
        .unwrap();
        assert_ne!(elliptical_ring.data(), old_circular_ring.data());
    }

    #[test]
    fn transformed_corner_radii_clamp_each_axis_independently() {
        assert_eq!(
            clamped_radii(Radii::new(80.0, 7.0), Bounds::new(0.0, 0.0, 100.0, 20.0)),
            Radii::new(50.0, 7.0)
        );
        assert_eq!(
            clamped_radii(Radii::new(8.0, 80.0), Bounds::new(0.0, 0.0, 100.0, 20.0)),
            Radii::new(8.0, 10.0)
        );
    }

    #[test]
    fn explicit_zero_radius_is_byte_identical_to_default() {
        let default = Rasterizer::new()
            .render(&fixture("same"), metrics())
            .unwrap();
        let caption = Node::new(
            NodeId::new("caption").unwrap(),
            Role::Text,
            "same",
            Bounds::new(3.0, 4.0, 250.0, 80.0),
            "--state-rest-surface",
        );
        let zero_scene = Scene::new(
            Node::new(
                NodeId::new("root").unwrap(),
                Role::Button,
                "same",
                Bounds::new(3.0, 4.0, 250.0, 80.0),
                "--state-rest-surface",
            )
            .with_action(NodeAction::Activate)
            .with_children(vec![caption])
            .with_corner_radius(0.0),
            NodeId::new("root").unwrap(),
        )
        .unwrap();
        let explicit = Rasterizer::new().render(&zero_scene, metrics()).unwrap();
        assert_eq!(default.rgba, explicit.rgba);
    }

    #[test]
    fn non_finite_corner_radius_has_stable_damage_and_paints_sharp() {
        let mut node = Node::new(
            NodeId::new("root").unwrap(),
            Role::Group,
            "",
            Bounds::new(20.0, 20.0, 40.0, 40.0),
            "--state-rest-surface",
        );
        node.corner_radius = f32::NAN;
        let scene = Scene::new(node, NodeId::new("root").unwrap()).unwrap();

        let mut rasterizer = Rasterizer::new();
        let first = rasterizer.render(&scene, metrics()).unwrap();
        let second = rasterizer.render(&scene, metrics()).unwrap();
        let sharp = Rasterizer::new()
            .render(&rounded_fixture(0.0), metrics())
            .unwrap();

        assert_eq!(second.damage, None);
        assert_eq!(first.rgba, sharp.rgba);
        assert_eq!(second.rgba, sharp.rgba);
    }

    #[test]
    fn rounded_image_content_is_clipped_to_node_silhouette() {
        let root = Node::new(
            NodeId::new("art").unwrap(),
            Role::ListItem,
            "",
            Bounds::new(20.0, 20.0, 40.0, 40.0),
            "--state-rest-surface",
        )
        .with_corner_radius(16.0)
        .with_image(
            ImageSource::new("rounded-art", Arc::<[u8]>::from(IMAGE_PNG)),
            ImageFit::Cover,
        );
        let scene = Scene::new(root, NodeId::new("art").unwrap()).unwrap();
        let frame = Rasterizer::new().render(&scene, metrics()).unwrap();
        assert_eq!(
            pixel(&frame, 20, 20),
            token_rgba(ThemeBase::Dusk, "--color-surface-canvas")
        );
        assert_ne!(
            pixel(&frame, 40, 40),
            token_rgba(ThemeBase::Dusk, "--color-surface-canvas")
        );
    }

    #[test]
    fn rounded_state_accents_are_clipped_to_node_silhouette() {
        let background = token_rgba(ThemeBase::Dusk, "--color-surface-canvas");
        let selected = token_rgba(ThemeBase::Dusk, "--state-selected-accent");
        let destructive = token_rgba(ThemeBase::Dusk, "--state-destructive-accent");

        for radius in [6.0, 10.0, 16.0, 25.0] {
            let selected_frame = Rasterizer::new()
                .render(
                    &state_scene(|node| {
                        node.corner_radius = radius;
                        node.state.selected = true;
                    }),
                    metrics(),
                )
                .unwrap();
            assert_eq!(
                pixel(&selected_frame, 20, 20),
                background,
                "radius {radius}"
            );
            assert_eq!(pixel(&selected_frame, 20, 45), selected, "radius {radius}");

            let destructive_frame = Rasterizer::new()
                .render(
                    &state_scene(|node| {
                        node.corner_radius = radius;
                        node.state.destructive = true;
                    }),
                    metrics(),
                )
                .unwrap();
            assert_eq!(
                pixel(&destructive_frame, 20, 20),
                background,
                "radius {radius}"
            );
            assert_eq!(
                pixel(&destructive_frame, 60, 20),
                destructive,
                "radius {radius}"
            );
        }
    }

    #[test]
    fn dusk_is_the_default_and_all_bases_are_selectable() {
        let scene = label_scene(Role::Text, "Theme");
        let default = Rasterizer::new().render(&scene, metrics()).unwrap();
        let mut explicit_dusk = Rasterizer::new();
        explicit_dusk.set_theme_base(ThemeBase::Dusk);
        assert_eq!(
            default.rgba,
            explicit_dusk.render(&scene, metrics()).unwrap().rgba
        );

        let mut high_contrast = Rasterizer::new();
        high_contrast.set_theme_base(ThemeBase::HighContrast);
        let frame = high_contrast.render(&scene, metrics()).unwrap();
        assert_eq!(pixel(&frame, 430, 276), [0, 0, 0, 255]);
        assert!(frame
            .rgba
            .chunks_exact(4)
            .any(|pixel| pixel == [255, 255, 255, 255]));

        let mut day = Rasterizer::new();
        day.set_theme_base(ThemeBase::Day);
        assert_eq!(
            pixel(&day.render(&scene, metrics()).unwrap(), 430, 276),
            [242, 238, 228, 255]
        );
    }

    #[test]
    fn theme_base_change_invalidates_damage_once_but_identical_set_does_not() {
        let scene = label_scene(Role::Text, "Theme");
        let surface = metrics();
        let full_surface = Some(DamageRect {
            x: 0,
            y: 0,
            width: surface.logical_width as u32,
            height: surface.logical_height as u32,
        });
        let mut rasterizer = Rasterizer::new();

        let dusk = rasterizer.render(&scene, surface).unwrap();
        assert_eq!(pixel(&dusk, 430, 276), [23, 21, 18, 255]);

        rasterizer.set_theme_base(ThemeBase::HighContrast);
        let changed = rasterizer.render(&scene, surface).unwrap();
        assert_eq!(changed.damage, full_surface);
        assert_eq!(pixel(&changed, 430, 276), [0, 0, 0, 255]);
        assert!(changed
            .rgba
            .chunks_exact(4)
            .any(|pixel| pixel == [255, 255, 255, 255]));

        assert_eq!(rasterizer.render(&scene, surface).unwrap().damage, None);
        rasterizer.set_theme_base(ThemeBase::HighContrast);
        assert_eq!(rasterizer.render(&scene, surface).unwrap().damage, None);
    }

    #[test]
    fn focused_row_uses_base_ring_on_base_canvas() {
        let mut root = Node::new(
            NodeId::new("focused").unwrap(),
            Role::Button,
            "",
            Bounds::new(3.0, 4.0, 250.0, 80.0),
            "--state-rest-surface",
        )
        .with_action(NodeAction::Activate);
        root.state.focused = true;
        let scene = Scene::new(root, NodeId::new("focused").unwrap()).unwrap();
        let dusk = Rasterizer::new().render(&scene, metrics()).unwrap();
        assert_eq!(pixel(&dusk, 430, 276), [23, 21, 18, 255]);
        assert!(dusk
            .rgba
            .chunks_exact(4)
            .any(|pixel| pixel == [243, 223, 174, 255]));

        let mut contrast = Rasterizer::new();
        contrast.set_theme_base(ThemeBase::HighContrast);
        let contrast = contrast.render(&scene, metrics()).unwrap();
        assert_eq!(pixel(&contrast, 430, 276), [0, 0, 0, 255]);
        assert!(contrast
            .rgba
            .chunks_exact(4)
            .any(|pixel| pixel == [255, 216, 61, 255]));
    }

    fn focus_scene(focused: bool) -> Scene {
        let root = Node::new(
            NodeId::new("focused").unwrap(),
            Role::Button,
            "",
            Bounds::new(20.0, 20.0, 40.0, 30.0),
            "--state-rest-surface",
        );
        let root = if focused {
            root.with_action(NodeAction::Activate)
        } else {
            root
        };
        Scene::new(root, NodeId::new("focused").unwrap()).unwrap()
    }

    fn pixel(frame: &RasterFrame, x: u32, y: u32) -> &[u8] {
        let start = ((y * frame.width + x) * 4) as usize;
        &frame.rgba[start..start + 4]
    }

    #[test]
    fn focus_ring_uses_outline_offset_geometry_in_dusk_and_high_contrast() {
        let cases = [
            (
                ThemeBase::Dusk,
                [23, 21, 18, 255],
                [243, 223, 174, 255],
                15,
                65,
            ),
            (
                ThemeBase::HighContrast,
                [0, 0, 0, 255],
                [255, 216, 61, 255],
                14,
                66,
            ),
        ];

        for (base, canvas, ring, outer_left, outer_right) in cases {
            let mut rasterizer = Rasterizer::new();
            rasterizer.set_theme_base(base);
            let frame = rasterizer.render(&focus_scene(true), metrics()).unwrap();

            // The outer edge is offset + width from the node's x range [20, 60).
            assert_eq!(pixel(&frame, outer_left, 35), ring, "{base:?} left edge");
            assert_eq!(
                pixel(&frame, outer_right - 1, 35),
                ring,
                "{base:?} right edge"
            );
            assert_eq!(
                pixel(&frame, outer_left - 1, 35),
                canvas,
                "{base:?} outside left edge"
            );
            assert_eq!(
                pixel(&frame, outer_right, 35),
                canvas,
                "{base:?} outside right edge"
            );

            // The three-pixel outline offset remains untouched canvas.
            for x in 17..20 {
                assert_eq!(pixel(&frame, x, 35), canvas, "{base:?} gap at x={x}");
            }
        }
    }

    #[test]
    fn focus_loss_damage_covers_ring_and_redraw_clears_it() {
        let mut rasterizer = Rasterizer::new();
        let focused = rasterizer.render(&focus_scene(true), metrics()).unwrap();
        assert_eq!(pixel(&focused, 15, 35), [243, 223, 174, 255]);

        let unfocused = rasterizer.render(&focus_scene(false), metrics()).unwrap();
        let damage = unfocused.damage.expect("focus loss is damaged");
        assert_eq!(damage.x, 0);
        assert_eq!(damage.y, 0);
        assert!(damage.width >= 92 && damage.height >= 82, "{damage:?}");
        assert_eq!(pixel(&unfocused, 15, 35), [23, 21, 18, 255]);

        let focused_again = rasterizer.render(&focus_scene(true), metrics()).unwrap();
        assert_eq!(focused_again.damage, unfocused.damage);
        assert_eq!(pixel(&focused_again, 15, 35), [243, 223, 174, 255]);
    }

    fn state_scene(configure: impl FnOnce(&mut Node)) -> Scene {
        let mut root = Node::new(
            NodeId::new("state").unwrap(),
            Role::Text,
            "MMMM",
            Bounds::new(20.0, 20.0, 80.0, 50.0),
            "--state-rest-surface",
        );
        configure(&mut root);
        Scene::new(root, NodeId::new("state").unwrap()).unwrap()
    }

    fn token_rgba(base: ThemeBase, key: &str) -> [u8; 4] {
        let style = pf_theme::flagship().resolved_style(base).unwrap();
        let color = style.color(key).unwrap();
        [color.red, color.green, color.blue, color.alpha]
    }

    #[test]
    fn all_state_and_scrim_primitives_resolve_per_base() {
        for base in [ThemeBase::Dusk, ThemeBase::Day, ThemeBase::HighContrast] {
            let render = |scene: &Scene| {
                let mut rasterizer = Rasterizer::new();
                rasterizer.set_theme_base(base);
                rasterizer.render(scene, metrics()).unwrap()
            };

            let selected = render(&state_scene(|n| n.state.selected = true));
            assert_eq!(
                pixel(&selected, 20, 45),
                token_rgba(base, "--state-selected-accent")
            );

            let destructive = render(&state_scene(|n| n.state.destructive = true));
            assert_eq!(
                pixel(&destructive, 50, 20),
                token_rgba(base, "--state-destructive-accent")
            );

            let disabled = render(&state_scene(|n| n.state.disabled = true));
            let disabled_text = token_rgba(base, "--state-disabled-text");
            assert!(disabled.rgba.chunks_exact(4).any(|p| p == disabled_text));
            assert_ne!(
                pixel(&disabled, 20, 45),
                token_rgba(base, "--state-rest-surface")
            );

            let unavailable = render(&state_scene(|n| n.state.unavailable = true));
            let surface = token_rgba(base, "--state-rest-surface");
            let veil = token_rgba(base, "--state-unavailable-veil");
            let expected = [
                ((u32::from(veil[0]) * u32::from(veil[3])
                    + u32::from(surface[0]) * (255 - u32::from(veil[3])))
                    / 255) as u8,
                ((u32::from(veil[1]) * u32::from(veil[3])
                    + u32::from(surface[1]) * (255 - u32::from(veil[3])))
                    / 255) as u8,
                ((u32::from(veil[2]) * u32::from(veil[3])
                    + u32::from(surface[2]) * (255 - u32::from(veil[3])))
                    / 255) as u8,
                255,
            ];
            assert!(
                pixel(&unavailable, 90, 60)
                    .iter()
                    .zip(expected)
                    .all(|(actual, expected)| actual.abs_diff(expected) <= 2),
                "{base:?}: actual={:?}, expected={expected:?}",
                pixel(&unavailable, 90, 60)
            );

            let scrimmed = render(&state_scene(|n| {
                n.style_token = "--color-status-ready".into();
                n.state.scrimmed = true;
            }));
            assert_ne!(
                pixel(&scrimmed, 90, 60),
                token_rgba(base, "--color-status-ready")
            );

            let pressed = render(&state_scene(|n| n.state.pressed = true));
            assert_eq!(
                pixel(&pressed, 20, 20),
                token_rgba(base, "--color-surface-canvas")
            );
            assert_eq!(pixel(&pressed, 21, 21), surface);
        }
    }

    fn pressed_composite(parent_pressed: bool, child_pressed: bool) -> Scene {
        let mut child = Node::new(
            NodeId::new("child").unwrap(),
            Role::Text,
            "MMMM",
            Bounds::new(40.0, 24.0, 20.0, 20.0),
            "--color-status-ready",
        );
        child.state.pressed = child_pressed;
        let mut root = Node::new(
            NodeId::new("root").unwrap(),
            Role::Button,
            "",
            Bounds::new(20.0, 20.0, 40.0, 30.0),
            "--state-rest-surface",
        )
        .with_action(NodeAction::Activate)
        .with_children(vec![child]);
        root.state.pressed = parent_pressed;
        Scene::new(root, NodeId::new("root").unwrap()).unwrap()
    }

    #[test]
    fn pressed_geometry_transforms_composite_subtree_and_release_restores_raster() {
        let mut rasterizer = Rasterizer::new();
        let rest = rasterizer
            .render(&pressed_composite(false, false), metrics())
            .unwrap();
        let pressed = rasterizer
            .render(&pressed_composite(true, false), metrics())
            .unwrap();
        let canvas = token_rgba(ThemeBase::Dusk, "--color-surface-canvas");
        let child_surface = token_rgba(ThemeBase::Dusk, "--color-status-ready");
        let child_text = token_rgba(ThemeBase::Dusk, "--state-rest-text");

        // The pressed parent occupies x=[21, 59). Its right-edge child and text must move
        // with that parent: no child ink remains at the rest-position edge x=59.
        assert_eq!(pixel(&pressed, 59, 30), canvas);
        assert!(
            (21..59).any(|x| pixel(&pressed, x, 30) == child_surface),
            "child surface did not render inside transformed parent"
        );
        assert!(
            (21..59).any(|x| (21..49).any(|y| pixel(&pressed, x, y) == child_text)),
            "child text did not render inside transformed parent"
        );
        let press_damage = pressed
            .damage
            .expect("press must damage transformed extents");
        assert!(press_damage.x <= 20 && press_damage.x + press_damage.width >= 60);
        assert!(press_damage.y <= 20 && press_damage.y + press_damage.height >= 50);

        let released = rasterizer
            .render(&pressed_composite(false, false), metrics())
            .unwrap();
        assert_eq!(
            released.rgba, rest.rgba,
            "release must restore the exact rest raster"
        );
        let release_damage = released
            .damage
            .expect("release must damage both pressed and rest extents");
        assert!(release_damage.x <= 20 && release_damage.x + release_damage.width >= 60);
        assert!(release_damage.y <= 20 && release_damage.y + release_damage.height >= 50);
    }

    #[test]
    fn nested_pressed_transforms_compose_in_the_paint_walk() {
        let shift = pf_theme::flagship()
            .resolved_style(ThemeBase::Dusk)
            .unwrap()
            .length("--state-pressed-shift")
            .unwrap()
            .pixels;
        let scene = pressed_composite(true, true);
        let root = scene.root();
        let parent = node_transform(root, LogicalTransform::IDENTITY, shift);
        let child = &root.children[0];
        let nested = node_transform(child, parent, shift);
        let parent_only = parent.map_bounds(child.bounds);
        let composed = nested.map_bounds(child.bounds);

        assert!(composed.x > parent_only.x);
        assert!(composed.y > parent_only.y);
        assert!(composed.width < parent_only.width);
        assert!(composed.height < parent_only.height);

        let parent_frame = Rasterizer::new()
            .render(&pressed_composite(true, false), metrics())
            .unwrap();
        let nested_frame = Rasterizer::new().render(&scene, metrics()).unwrap();
        assert_ne!(nested_frame.rgba, parent_frame.rgba);
    }

    fn degenerate_pressed_composite(width: f32, height: f32, pressed: bool) -> Scene {
        let child = Node::new(
            NodeId::new("child").unwrap(),
            Role::Text,
            "child",
            Bounds::new(40.0, 30.0, 20.0, 12.0),
            "--color-status-ready",
        );
        let mut root = Node::new(
            NodeId::new("root").unwrap(),
            Role::Button,
            "",
            Bounds::new(20.0, 20.0, width, height),
            "--state-rest-surface",
        )
        .with_action(NodeAction::Activate)
        .with_children(vec![child]);
        root.state.pressed = pressed;
        Scene::new(root, NodeId::new("root").unwrap()).unwrap()
    }

    #[test]
    fn degenerate_pressed_axes_keep_descendant_transforms_finite_and_at_rest() {
        let shift = pf_theme::flagship()
            .resolved_style(ThemeBase::Dusk)
            .unwrap()
            .length("--state-pressed-shift")
            .unwrap()
            .pixels;

        for (width, height) in [(0.0, 30.0), (40.0, 0.0)] {
            let scene = degenerate_pressed_composite(width, height, true);
            let root = scene.root();
            let transform = node_transform(root, LogicalTransform::IDENTITY, shift);
            let child = root.children[0].bounds;
            let mapped = transform.map_bounds(child);

            assert!(transform.is_finite());
            assert!([mapped.x, mapped.y, mapped.width, mapped.height]
                .into_iter()
                .all(f32::is_finite));
            if width == 0.0 {
                assert_eq!((mapped.x, mapped.width), (child.x, child.width));
            }
            if height == 0.0 {
                assert_eq!((mapped.y, mapped.height), (child.y, child.height));
            }
        }
    }

    #[test]
    fn degenerate_pressed_nodes_keep_descendants_visible_and_damage_bounded() {
        let child_surface = token_rgba(ThemeBase::Dusk, "--color-status-ready");
        for (width, height) in [(0.0, 30.0), (40.0, 0.0)] {
            let mut rasterizer = Rasterizer::new();
            rasterizer
                .render(
                    &degenerate_pressed_composite(width, height, false),
                    metrics(),
                )
                .unwrap();
            let pressed = rasterizer
                .render(
                    &degenerate_pressed_composite(width, height, true),
                    metrics(),
                )
                .unwrap();

            assert!(
                pressed
                    .rgba
                    .chunks_exact(4)
                    .any(|pixel| pixel == child_surface),
                "descendant disappeared for {width}x{height} pressed parent"
            );
            let damage = pressed.damage.expect("press must produce finite damage");
            assert!(damage.x <= pressed.width && damage.y <= pressed.height);
            assert!(damage.x + damage.width <= pressed.width);
            assert!(damage.y + damage.height <= pressed.height);
        }
    }

    #[test]
    fn prebaked_nine_slices_are_stable_and_high_contrast_depth_is_absent() {
        for base in [ThemeBase::Dusk, ThemeBase::Day, ThemeBase::HighContrast] {
            for elevation in [Elevation::Elev1, Elevation::Elev2, Elevation::Focus] {
                let first = prebaked_elevation_bytes(base, elevation);
                let second = prebaked_elevation_bytes(base, elevation);
                assert_eq!(first, second);
                assert_eq!(first.len() % 4, 0);
                if base == ThemeBase::HighContrast {
                    assert!(first.iter().all(|byte| *byte == 0));
                } else {
                    assert!(first.iter().any(|byte| *byte != 0));
                }
            }
        }

        let plain = state_scene(|_| {});
        let elevated = state_scene(|n| n.elevation = Elevation::Elev2);
        let mut rasterizer = Rasterizer::new();
        rasterizer.set_theme_base(ThemeBase::HighContrast);
        let plain = rasterizer.render(&plain, metrics()).unwrap().rgba;
        let elevated = rasterizer.render(&elevated, metrics()).unwrap().rgba;
        assert_eq!(plain, elevated);
    }

    #[test]
    fn high_contrast_rounded_elevation_skips_shadow_cache() {
        let mut node = Node::new(
            NodeId::new("rounded").unwrap(),
            Role::Group,
            "",
            Bounds::new(20.0, 20.0, 40.0, 40.0),
            "--state-rest-surface",
        )
        .with_corner_radius(16.0)
        .with_elevation(Elevation::Elev2);
        node.state.focused = true;
        let scene = Scene::new(node, NodeId::new("rounded").unwrap()).unwrap();
        let mut rasterizer = Rasterizer::new();
        rasterizer.set_theme_base(ThemeBase::HighContrast);

        rasterizer.render(&scene, metrics()).unwrap();

        assert!(rasterizer.rounded_shadows.assets.is_empty());
        assert!(rasterizer.rounded_shadows.recency.is_empty());
    }

    #[test]
    fn rounded_shadow_bakes_from_radius_silhouette_and_keeps_straight_penumbra() {
        for elevation in [Elevation::Elev1, Elevation::Elev2, Elevation::Focus] {
            let square = shadow_asset(ThemeBase::Dusk, elevation);
            for radius in [16usize, 20] {
                let rounded = bake_rounded_shadow(square, radius);
                let alpha =
                    |rgba: &[u8], side: usize, x: usize, y: usize| rgba[(y * side + x) * 4 + 3];

                // The old square source has its strongest corner contribution here. A rounded
                // source removes that corner before blur, including for a 40px pill (r=20).
                let square_corner = alpha(
                    square.rgba,
                    square.side,
                    square.margin + 1,
                    square.margin + 1,
                );
                let rounded_corner =
                    alpha(&rounded.rgba, rounded.width, square.margin, square.margin);
                assert!(
                    rounded_corner <= 8 && rounded_corner <= square_corner,
                    "{elevation:?} r={radius}: rounded={rounded_corner}, square={square_corner}"
                );

                // Far from the corner, the rounded mask is the same half-plane as the square
                // mask, so its blur must retain the existing straight-edge penumbra profile.
                let square_edge = alpha(square.rgba, square.side, square.side / 2, square.margin);
                let rounded_edge = alpha(
                    &rounded.rgba,
                    rounded.width,
                    rounded.width / 2,
                    square.margin,
                );
                assert!(
                    rounded_edge.abs_diff(square_edge) <= 1,
                    "{elevation:?} r={radius}: rounded={rounded_edge}, square={square_edge}"
                );
            }
        }
    }

    #[test]
    fn non_uniform_pressed_shadow_uses_elliptical_silhouette() {
        let asset = shadow_asset(ThemeBase::Dusk, Elevation::Elev2);
        let bounds = Bounds::new(21.0, 21.0, 98.0, 18.0);
        let mut elliptical = Pixmap::new(140, 60).unwrap();
        draw_node_shadow(
            &mut elliptical,
            &mut RoundedShadowCache::default(),
            asset,
            bounds,
            1.0,
            Radii::new(15.68, 9.0),
        );
        let mut old_circle = Pixmap::new(140, 60).unwrap();
        draw_node_shadow(
            &mut old_circle,
            &mut RoundedShadowCache::default(),
            asset,
            bounds,
            1.0,
            Radii::new(9.0, 9.0),
        );
        assert_ne!(elliptical.data(), old_circle.data());

        let baked = bake_rounded_shadow_physical(asset, Radii::new(16.0, 9.0), asset.margin);
        assert!(baked.width > baked.height);
        assert!(baked.slice_margin_x > baked.slice_margin_y);
    }

    #[test]
    fn zero_radius_shadow_draw_remains_byte_identical_to_prebaked_asset_path() {
        let asset = shadow_asset(ThemeBase::Dusk, Elevation::Elev2);
        let bounds = Bounds::new(32.0, 32.0, 40.0, 40.0);
        let mut expected = Pixmap::new(112, 112).unwrap();
        draw_shadow(
            &mut expected,
            asset.rgba,
            asset.side,
            asset.margin,
            asset.margin,
            bounds,
            1.0,
        );
        let mut actual = Pixmap::new(112, 112).unwrap();
        draw_node_shadow(
            &mut actual,
            &mut RoundedShadowCache::default(),
            asset,
            bounds,
            1.0,
            Radii::new(0.0, 0.0),
        );
        assert_eq!(actual.data(), expected.data());
    }

    #[test]
    fn rounded_shadow_radius_is_bounded_by_physical_extent_and_ceiling() {
        let degenerate = Bounds::new(0.0, 0.0, 100_000.0, 100_000.0);
        assert_eq!(
            quantized_physical_shadow_radii(degenerate, 0.001, Radii::new(100.0, 100.0)),
            (50, 50)
        );
        let huge = Bounds::new(0.0, 0.0, 100_000.0, 100_000.0);
        assert_eq!(
            quantized_physical_shadow_radii(huge, 1.0, Radii::new(50_000.0, 50_000.0)),
            (MAX_ROUNDED_SHADOW_RADIUS, MAX_ROUNDED_SHADOW_RADIUS)
        );
        let asset = bake_rounded_shadow(
            shadow_asset(ThemeBase::Dusk, Elevation::Elev1),
            quantized_physical_shadow_radii(degenerate, 0.001, Radii::new(100.0, 100.0)).0 as usize,
        );
        assert_eq!(asset.width, asset.effect_margin * 2 + 50 * 2 + 3);
        assert_eq!(asset.height, asset.width);
    }

    #[test]
    fn rounded_shadow_cache_evicts_under_radius_churn_and_reuses_tokens() {
        let asset = shadow_asset(ThemeBase::Dusk, Elevation::Elev1);
        let mut cache = RoundedShadowCache::default();
        for radius in 1..=(ROUNDED_SHADOW_CACHE_CAPACITY as u32 * 4) {
            cache.get_or_bake((0, 1, radius, radius, asset.margin as u16), asset);
            assert!(cache.assets.len() <= ROUNDED_SHADOW_CACHE_CAPACITY);
        }
        for scale in [1.0, 2.0] {
            for logical_radius in [6.0, 10.0, 16.0] {
                let physical = (logical_radius * scale) as u32;
                let margin = (asset.margin as f32 * scale) as u16;
                cache.get_or_bake((0, 1, physical, physical, margin), asset);
                let len = cache.assets.len();
                cache.get_or_bake((0, 1, physical, physical, margin), asset);
                assert_eq!(cache.assets.len(), len);
            }
        }
        assert_eq!(cache.assets.len(), ROUNDED_SHADOW_CACHE_CAPACITY);
        assert_eq!(cache.assets.len(), cache.recency.len());
    }

    #[test]
    fn token_radius_shadow_samples_match_round_three_at_common_scales() {
        let asset = shadow_asset(ThemeBase::Dusk, Elevation::Elev2);
        let bounds = Bounds::new(32.0, 32.0, 80.0, 80.0);
        for scale in [1.0, 2.0] {
            for logical_radius in [6usize, 10, 16, 40] {
                let side = (128.0 * scale) as u32;
                let mut round_three = Pixmap::new(side, side).unwrap();
                let old = bake_rounded_shadow(asset, logical_radius);
                draw_shadow(
                    &mut round_three,
                    &old.rgba,
                    old.width,
                    old.slice_margin_x,
                    old.effect_margin,
                    bounds,
                    scale,
                );

                let mut current = Pixmap::new(side, side).unwrap();
                draw_node_shadow(
                    &mut current,
                    &mut RoundedShadowCache::default(),
                    asset,
                    bounds,
                    scale,
                    Radii::new(logical_radius as f32 * scale, logical_radius as f32 * scale),
                );
                for (logical_x, logical_y) in [(72.0, 32.0), (32.0, 72.0), (72.0, 72.0)] {
                    let x = (logical_x * scale) as u32;
                    let y = (logical_y * scale) as u32;
                    assert_eq!(
                        current.pixel(x, y),
                        round_three.pixel(x, y),
                        "scale={scale} radius={logical_radius} sample=({x},{y})"
                    );
                }
            }
        }
    }

    #[test]
    fn nine_slice_edge_texels_keep_scaled_source_provenance() {
        let side = 7usize;
        let margin = 2usize;
        let mut rgba = Vec::with_capacity(side * side * 4);
        for _y in 0..side {
            for x in 0..side {
                rgba.extend_from_slice(&[(x * 30) as u8, 0, 0, 255]);
            }
        }
        let asset = ShadowAsset {
            base: ThemeBase::Dusk,
            elevation: Elevation::Elev1,
            side,
            margin,
            offset_x: 0.0,
            offset_y: 0.0,
            blur: 0.0,
            spread: 0.0,
            color: [0, 0, 0, 255],
            rgba: Box::leak(rgba.into_boxed_slice()),
        };

        let raster = |scale: f32| {
            let mut pm = Pixmap::new(64, 32).unwrap();
            draw_shadow(
                &mut pm,
                asset.rgba,
                asset.side,
                asset.margin,
                asset.margin,
                Bounds::new(10.0, 10.0, 12.0, 8.0),
                scale,
            );
            pm
        };
        let half = raster(0.5);
        let one = raster(1.0);
        let two = raster(2.0);

        // At 0.5x the single destination edge pixel reaches the first source edge.
        assert_eq!(half.pixel(4, 7).unwrap().demultiply().red(), 0);
        // At 1x source edge texels 0 and 1 are preserved one-for-one.
        assert_eq!(one.pixel(8, 14).unwrap().demultiply().red(), 0);
        assert_eq!(one.pixel(9, 14).unwrap().demultiply().red(), 30);
        // At 2x each source edge texel occupies two pixels; none comes from center x=3.
        assert_eq!(two.pixel(16, 28).unwrap().demultiply().red(), 0);
        assert_eq!(two.pixel(17, 28).unwrap().demultiply().red(), 0);
        assert_eq!(two.pixel(18, 28).unwrap().demultiply().red(), 30);
        assert_eq!(two.pixel(19, 28).unwrap().demultiply().red(), 30);
    }

    #[test]
    fn non_color_state_glyphs_render_in_dusk_and_high_contrast() {
        for base in [ThemeBase::Dusk, ThemeBase::HighContrast] {
            let render = |scene: &Scene| {
                let mut rasterizer = Rasterizer::new();
                rasterizer.set_theme_base(base);
                rasterizer.render(scene, metrics()).unwrap()
            };
            let unavailable = render(&state_scene(|n| n.state.unavailable = true));
            let slash = token_rgba(base, "--state-unavailable-text");
            assert!((80..95).any(|x| (25..40).any(|y| pixel(&unavailable, x, y) == slash)));

            let destructive = render(&state_scene(|n| n.state.destructive = true));
            let warning = token_rgba(base, "--state-destructive-accent");
            assert!((80..95).any(|x| (25..40).any(|y| pixel(&destructive, x, y) == warning)));
        }
    }

    #[test]
    fn disabled_image_only_nodes_keep_renderer_owned_em_dash_ink() {
        for base in [ThemeBase::Dusk, ThemeBase::HighContrast] {
            let scene = state_scene(|n| {
                n.accessible_label.clear();
                n.state.disabled = true;
                *n = n.clone().with_image(
                    ImageSource::new("disabled-image", Arc::<[u8]>::from(IMAGE_PNG)),
                    ImageFit::Cover,
                );
            });
            let mut rasterizer = Rasterizer::new();
            rasterizer.set_theme_base(base);
            let disabled = rasterizer.render(&scene, metrics()).unwrap();
            let dash = token_rgba(base, "--state-disabled-text");

            assert!(
                (80..95).any(|x| (25..40).any(|y| pixel(&disabled, x, y) == dash)),
                "{base:?} disabled image lost its em-dash"
            );
        }
    }

    #[test]
    fn disabled_labeled_nodes_render_border_dash_and_muted_text() {
        for base in [ThemeBase::Dusk, ThemeBase::HighContrast] {
            let mut rasterizer = Rasterizer::new();
            rasterizer.set_theme_base(base);
            let disabled = rasterizer
                .render(&state_scene(|n| n.state.disabled = true), metrics())
                .unwrap();
            let surface = token_rgba(base, "--state-rest-surface");
            let muted = token_rgba(base, "--state-disabled-text");

            assert!(
                (20..100).any(|x| pixel(&disabled, x, 20) != surface)
                    && (20..100).any(|x| pixel(&disabled, x, 20) == surface),
                "{base:?} disabled label lost its dashed border"
            );
            assert!(
                (80..95).any(|x| (25..40).any(|y| pixel(&disabled, x, y) == muted)),
                "{base:?} disabled label lost its em-dash"
            );
            assert!(
                (26..78).any(|x| (25..65).any(|y| pixel(&disabled, x, y) == muted)),
                "{base:?} disabled label lost its muted text"
            );
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "unknown or non-color style key")]
    fn unknown_node_style_key_fails_loudly_in_debug() {
        let root = Node::new(
            NodeId::new("unknown").unwrap(),
            Role::Button,
            "",
            Bounds::new(3.0, 4.0, 250.0, 80.0),
            "--not-a-real-style",
        );
        Rasterizer::new()
            .render(
                &Scene::new(root, NodeId::new("unknown").unwrap()).unwrap(),
                metrics(),
            )
            .unwrap();
    }

    fn image_fixture(bytes: &'static [u8], fit: ImageFit) -> Scene {
        let root = Node::new(
            NodeId::new("root").unwrap(),
            Role::Button,
            "cover art",
            Bounds::new(17.0, 11.0, 173.0, 129.0),
            "--state-rest-surface",
        )
        .with_action(NodeAction::Activate)
        .with_image(
            ImageSource::new("fixture-v1", Arc::<[u8]>::from(bytes)),
            fit,
        );
        Scene::new(root, NodeId::new("root").unwrap()).unwrap()
    }

    fn frame_hash(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        })
    }

    #[test]
    fn image_frame_hash_is_deterministic_across_fresh_runs() {
        let first = Rasterizer::new()
            .render(&image_fixture(IMAGE_PNG, ImageFit::Cover), metrics())
            .unwrap();
        let second = Rasterizer::new()
            .render(&image_fixture(IMAGE_PNG, ImageFit::Cover), metrics())
            .unwrap();
        assert!(first.notes.is_empty());
        assert_eq!(frame_hash(&first.rgba), frame_hash(&second.rgba));
        assert_eq!(frame_hash(&first.rgba), 4_902_161_646_795_392_738);
    }

    #[test]
    fn corrupt_image_is_a_typed_note_and_never_draws_debug_text() {
        let frame = Rasterizer::new()
            .render(&image_fixture(CORRUPT_PNG, ImageFit::Contain), metrics())
            .unwrap();
        assert_eq!(
            frame.notes,
            vec![RenderNote::ImageDecodeFailed {
                source_id: "fixture-v1".into()
            }]
        );
        // The node's themed background remains; its semantic alt text is not painted.
        assert!(!frame
            .rgba
            .chunks_exact(4)
            .any(|p| p == [244, 234, 220, 255]));
    }

    #[test]
    fn oversized_header_is_rejected_before_pixel_allocation() {
        let mut bytes = vec![0; 24];
        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        bytes[12..16].copy_from_slice(b"IHDR");
        bytes[16..20].copy_from_slice(&4_000u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&3_000u32.to_be_bytes());
        let source = ImageSource::new("oversized", Arc::<[u8]>::from(bytes));
        assert_eq!(
            decode_png(&source).unwrap_err(),
            RenderNote::ImageTooLarge {
                source_id: "oversized".into(),
                width: 4_000,
                height: 3_000,
                max_pixels: MAX_IMAGE_PIXELS,
            }
        );
    }

    #[test]
    fn image_content_and_fit_participate_in_damage_tracking() {
        let mut rasterizer = Rasterizer::new();
        rasterizer
            .render(&image_fixture(IMAGE_PNG, ImageFit::Contain), metrics())
            .unwrap();
        let frame = rasterizer
            .render(&image_fixture(IMAGE_PNG, ImageFit::Cover), metrics())
            .unwrap();
        let damage = frame.damage.expect("fit change is damaged");
        assert_eq!((damage.x, damage.y), (0, 0));
        assert!(damage.width >= 222 && damage.height >= 172, "{damage:?}");
    }
    #[test]
    fn cjk_fixture_produces_ink_and_damage_accumulates() {
        let mut r = Rasterizer::new();
        let first = r.render(&fixture("日本語"), metrics()).unwrap();
        assert!(first
            .rgba
            .chunks_exact(4)
            .any(|p| { p[0] > 80 && p[1] > 80 && p[2] > 80 && p != [13, 17, 23, 255] }));
        assert!(first.damage.is_some());
        let second = r.render(&fixture("日本語"), metrics()).unwrap();
        assert_eq!(second.damage, None);
    }

    #[test]
    fn semantic_button_with_explicit_caption_matches_text_fixture() {
        let bounds = Bounds::new(7.0, 9.0, 120.0, 51.0);
        let text = Node::new(
            NodeId::new("card").unwrap(),
            Role::Text,
            "続ける",
            bounds,
            "--state-rest-surface",
        );
        let expected = Scene::new(text, NodeId::new("card").unwrap()).unwrap();

        let caption = Node::new(
            NodeId::new("caption").unwrap(),
            Role::Text,
            "続ける",
            bounds,
            "--state-rest-surface",
        );
        let button = Node::new(
            NodeId::new("card").unwrap(),
            Role::Button,
            "続ける",
            bounds,
            "--state-rest-surface",
        )
        .with_children(vec![caption]);
        let actual = Scene::new(button, NodeId::new("card").unwrap()).unwrap();

        assert_eq!(
            Rasterizer::new().render(&actual, metrics()).unwrap().rgba,
            Rasterizer::new()
                .render(&expected, metrics())
                .unwrap()
                .rgba
        );
    }

    fn overlapping_scene(order: [&str; 2]) -> Scene {
        let children = order.map(|id| {
            Node::new(
                NodeId::new(id).unwrap(),
                Role::Text,
                id,
                if id == "front" {
                    Bounds::new(20.0, 20.0, 80.0, 40.0)
                } else {
                    Bounds::new(50.0, 30.0, 80.0, 40.0)
                },
                "--state-rest-surface",
            )
        });
        let root = Node::new(
            NodeId::new("root").unwrap(),
            Role::Button,
            "",
            Bounds::new(0.0, 0.0, 150.0, 90.0),
            "--state-rest-surface",
        )
        .with_action(NodeAction::Activate)
        .with_children(children.into());
        Scene::new(root, NodeId::new("root").unwrap()).unwrap()
    }

    #[test]
    fn swapping_overlapping_siblings_damages_their_union() {
        let mut r = Rasterizer::new();
        r.render(&overlapping_scene(["front", "back"]), metrics())
            .unwrap();
        let frame = r
            .render(&overlapping_scene(["back", "front"]), metrics())
            .unwrap();
        assert_eq!(
            frame.damage,
            Some(DamageRect {
                x: 20,
                y: 20,
                width: 110,
                height: 50,
            })
        );
    }

    #[test]
    fn wrapping_text_is_clipped_to_node_bounds() {
        let bounds = Bounds::new(40.0, 30.0, 42.0, 24.0);
        let caption = Node::new(
            NodeId::new("caption").unwrap(),
            Role::Text,
            "This label wraps across far more lines than fit",
            bounds,
            "--state-rest-surface",
        );
        let node = Node::new(
            NodeId::new("root").unwrap(),
            Role::Button,
            "This label wraps across far more lines than fit",
            bounds,
            "--state-rest-surface",
        )
        .with_action(NodeAction::Activate)
        .with_children(vec![caption]);
        let scene = Scene::new(node, NodeId::new("root").unwrap()).unwrap();
        let frame = Rasterizer::new().render(&scene, metrics()).unwrap();
        for y in 0..frame.height {
            for x in 0..frame.width {
                // Focus paint extends five pixels beyond the node; text must not.
                if !(35..87).contains(&x) || !(25..59).contains(&y) {
                    let offset = (y * frame.width + x) as usize * 4;
                    assert_eq!(&frame.rgba[offset..offset + 4], &[23, 21, 18, 255]);
                }
            }
        }
    }

    #[test]
    fn role_metrics_are_resolved_exactly_from_the_vendored_tokens() {
        let typography = TypographySnapshot::flagship();
        let expected = [
            (TypeRole::Hero, 52.0, 800, 0.0),
            (TypeRole::Title, 34.0, 800, 0.0),
            (TypeRole::H1, 22.0, 700, 0.0),
            (TypeRole::Body, 15.0, 500, 0.0),
            (TypeRole::Label, 14.0, 600, 0.0),
            (TypeRole::Caption, 12.5, 600, 0.0),
            (TypeRole::Eyebrow, 11.5, 700, 0.14),
        ];
        for (role, size_px, weight, tracking_em) in expected {
            let style = typography.resolve(role);
            assert_eq!(style.family, "Manrope", "{role:?}");
            assert_eq!(style.size_px, size_px, "{role:?}");
            assert_eq!(style.weight, weight, "{role:?}");
            assert_eq!(style.tracking_em, tracking_em, "{role:?}");
        }
    }

    #[test]
    fn fraunces_is_exclusively_bound_to_plate_text() {
        let typography = TypographySnapshot::flagship();
        for role in [
            TypeRole::Hero,
            TypeRole::Title,
            TypeRole::H1,
            TypeRole::Body,
            TypeRole::Label,
            TypeRole::Caption,
            TypeRole::Eyebrow,
        ] {
            assert_ne!(typography.resolve(role).family, "Fraunces", "{role:?}");
        }
        assert_eq!(typography.resolve(TypeRole::Plate).family, "Fraunces");
    }

    fn copy_fixture() -> Scene {
        let root = Node::new(
            NodeId::new("copy").unwrap(),
            Role::Text,
            "Measured body text wraps into additional lines as its accessible text scale grows",
            Bounds::new(8.0, 8.0, 150.0, 230.0),
            "--state-rest-surface",
        )
        .with_type_role(TypeRole::Body)
        .with_line_height(1.5);
        Scene::new(root, NodeId::new("copy").unwrap()).unwrap()
    }

    fn text_ink(frame: &RasterFrame) -> Vec<(u32, u32)> {
        let text = [244, 239, 230, 255];
        (0..frame.height)
            .flat_map(|y| (0..frame.width).map(move |x| (x, y)))
            .filter(|&(x, y)| pixel(frame, x, y) == text)
            .collect()
    }

    #[test]
    fn text_scale_reshapes_and_reflows_body_copy_instead_of_scaling_a_framebuffer() {
        let scene = copy_fixture();
        let mut normal = Rasterizer::new();
        let one = normal.render(&scene, metrics()).unwrap();
        let mut large = Rasterizer::new();
        large.set_text_scale(2.0).unwrap();
        let two = large.render(&scene, metrics()).unwrap();
        assert_eq!((one.width, one.height), (two.width, two.height));
        let one_ink = text_ink(&one);
        let two_ink = text_ink(&two);
        assert!(two_ink.len() > one_ink.len());
        let one_bottom = one_ink.iter().map(|p| p.1).max().unwrap();
        let two_bottom = two_ink.iter().map(|p| p.1).max().unwrap();
        assert!(
            two_bottom > one_bottom * 3 / 2,
            "{one_bottom} -> {two_bottom}"
        );
    }

    #[test]
    fn cached_glyph_raster_is_deterministic_for_same_text_role_and_scale() {
        let scene = copy_fixture();
        let mut rasterizer = Rasterizer::new();
        rasterizer.set_text_scale(2.0).unwrap();
        let cold = rasterizer.render(&scene, metrics()).unwrap();
        let cached = rasterizer.render(&scene, metrics()).unwrap();
        assert_eq!(cold.rgba, cached.rgba);
        assert_eq!(cached.damage, None);
    }

    #[test]
    fn unified_body_raster_is_stable() {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let frame = Rasterizer::new()
            .render(&copy_fixture(), metrics())
            .unwrap();
        let mut hasher = DefaultHasher::new();
        frame.rgba.hash(&mut hasher);
        assert_eq!(hasher.finish(), 0xa401_a750_e354_68a1);
    }

    fn raster_weight(weight: u16, glyphs: &mut tracked_text::SwashCache) -> (u64, usize) {
        let mut db = tracked_text::fontdb::Database::new();
        db.load_font_data(MANROPE.to_vec());
        let mut fonts = tracked_text::FontSystem::new_with_locale_and_db("en-US".into(), db);
        let style = ResolvedTypeStyle {
            family: "Manrope".into(),
            size_px: 32.0,
            weight,
            tracking_em: 0.0,
        };
        let mut pixmap = Pixmap::new(300, 60).unwrap();
        draw_text(
            &mut pixmap,
            &mut fonts,
            glyphs,
            TextDraw {
                text: "Variable Weight",
                x: 0.0,
                y: 0.0,
                width: 300.0,
                height: 60.0,
                style: &style,
                text_scale: 1.0,
                surface_scale: 1.0,
                line_height: None,
                clip: (0.0, 0.0, 300.0, 60.0),
                color: Rgba {
                    red: 255,
                    green: 255,
                    blue: 255,
                    alpha: 255,
                },
            },
        );
        let ink_mass = pixmap
            .pixels()
            .iter()
            .map(|pixel| u64::from(pixel.demultiply().red()))
            .sum();
        (ink_mass, glyphs.image_cache.len())
    }

    #[test]
    fn variable_role_weights_produce_heavier_pixels_at_equal_size() {
        let body = raster_weight(500, &mut tracked_text::SwashCache::new()).0;
        let h1 = raster_weight(700, &mut tracked_text::SwashCache::new()).0;
        let hero = raster_weight(800, &mut tracked_text::SwashCache::new()).0;
        assert!(h1 > body, "h1(700) ink mass {h1} <= body(500) {body}");
        assert!(hero > body, "hero(800) ink mass {hero} <= body(500) {body}");
    }

    #[test]
    fn glyph_cache_keeps_distinct_entries_for_distinct_weights() {
        let mut glyphs = tracked_text::SwashCache::new();
        let (_, body_entries) = raster_weight(500, &mut glyphs);
        let (_, hero_entries) = raster_weight(800, &mut glyphs);
        assert!(body_entries > 0);
        assert!(
            hero_entries > body_entries,
            "weight-specific glyphs aliased in the cache: {body_entries} -> {hero_entries}"
        );
        let weights: std::collections::HashSet<_> = glyphs
            .image_cache
            .keys()
            .map(|key| key.font_weight.0)
            .collect();
        assert!(weights.contains(&500));
        assert!(weights.contains(&800));
    }

    fn tracked_layout(text: &str, width: f32, size: f32, spacing_em: f32) -> Vec<(f32, Vec<f32>)> {
        let mut db = tracked_text::fontdb::Database::new();
        db.load_font_data(MANROPE.to_vec());
        let mut fonts = tracked_text::FontSystem::new_with_locale_and_db("en-US".into(), db);
        let mut buffer =
            tracked_text::Buffer::new(&mut fonts, tracked_text::Metrics::new(size, size * 1.25));
        buffer.set_size(&mut fonts, Some(width), Some(200.0));
        buffer.set_text(
            &mut fonts,
            text,
            &tracked_text::Attrs::new()
                .family(tracked_text::Family::Name("Manrope"))
                .weight(tracked_text::Weight(700))
                .letter_spacing(spacing_em),
            tracked_text::Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(&mut fonts, false);
        buffer
            .layout_runs()
            .map(|run| (run.line_w, run.glyphs.iter().map(|glyph| glyph.w).collect()))
            .collect()
    }

    #[test]
    fn eyebrow_advance_is_exactly_natural_plus_token_tracking() {
        let text = "READY NOW · 3";
        let natural = tracked_layout(text, 1_000.0, 11.5, 0.0);
        let tracked = tracked_layout(text, 1_000.0, 11.5, 0.14);
        assert_eq!(natural.len(), 1);
        assert_eq!(tracked.len(), 1);
        let (measured, advances) = &tracked[0];
        let natural_advances = &natural[0].1;
        assert_eq!(advances.len(), natural_advances.len());
        for (index, (advance, natural_advance)) in advances.iter().zip(natural_advances).enumerate()
        {
            let expected = natural_advance + 0.14 * 11.5;
            assert!(
                (advance - expected).abs() < 0.001,
                "glyph {index}: {advance} != {natural_advance} + {}",
                0.14 * 11.5
            );
        }
        let advance_sum: f32 = advances.iter().sum();
        assert!(
            (measured - advance_sum).abs() < 0.001,
            "{measured} != {advance_sum}"
        );
        assert!(
            (70.0..=110.0).contains(measured),
            "eyebrow width {measured}px is outside the ruled band"
        );
    }

    #[test]
    fn eyebrow_fits_220px_on_one_line_and_text_scale_doubles_width() {
        let text = "READY NOW · 3";
        let scale_one = tracked_layout(text, 220.0, 11.5, 0.14);
        let scale_two = tracked_layout(text, 440.0, 23.0, 0.14);
        assert_eq!(scale_one.len(), 1);
        assert_eq!(scale_two.len(), 1);
        assert!((scale_two[0].0 - scale_one[0].0 * 2.0).abs() < 0.001);
    }

    #[test]
    fn untracked_roles_keep_byte_identical_advances() {
        for role in [TypeRole::Body, TypeRole::Label] {
            let style = parse_type_role(role);
            assert_eq!(style.tracking_em, 0.0);
            let baseline = tracked_layout("Baseline text", 1_000.0, style.size_px, 0.0);
            let resolved =
                tracked_layout("Baseline text", 1_000.0, style.size_px, style.tracking_em);
            assert_eq!(resolved, baseline, "{role:?} advances changed");
        }
    }
}
