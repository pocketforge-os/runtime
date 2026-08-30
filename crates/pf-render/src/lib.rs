//! Deterministic scene rasterization on the ruled Cosmic Text/Swash/tiny-skia stack.
//!
//! All paint colors are resolved from the active `pf-theme` base at presentation time.

use cosmic_text_tracking as tracked_text;
use pf_scene::{Bounds, ImageFit, ImageSource, Node, NodeContent, Scene, SurfaceMetrics, TypeRole};
pub use pf_theme::Base as ThemeBase;
use pf_theme::{ResolvedStyleSnapshot, Rgba};
use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use tiny_skia::{
    Color as SkColor, FilterQuality, Mask, Paint, PathBuilder, Pixmap, PixmapPaint, Rect, Stroke,
    Transform,
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
    theme_base: ThemeBase,
    style: ResolvedStyleSnapshot,
    typography: TypographySnapshot,
    text_scale: f32,
}

#[derive(Clone, PartialEq)]
struct NodeSnapshot {
    id: String,
    parent_id: Option<String>,
    sibling_index: usize,
    bounds: Bounds,
    label: String,
    style_token: String,
    type_role: TypeRole,
    line_height: Option<f32>,
    focused: bool,
    disabled: bool,
    selected: bool,
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
            notes: &mut notes,
            style: &self.style,
            typography: &self.typography,
            text_scale: self.text_scale,
        };
        draw_node(&mut pixmap, &mut context, scene.root(), metrics.scale)?;
        let mut current = Vec::new();
        collect(scene.root(), None, 0, &mut current);
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
) {
    out.push(NodeSnapshot {
        id: node.id.as_str().into(),
        parent_id: parent_id.map(str::to_owned),
        sibling_index,
        bounds: node.bounds,
        label: node.accessible_label.clone(),
        style_token: node.style_token.clone(),
        type_role: node.type_role,
        line_height: node.line_height,
        focused: node.state.focused,
        disabled: node.state.disabled,
        selected: node.state.selected,
        content: match &node.content {
            NodeContent::Label => ContentSnapshot::Label,
            NodeContent::Image { source, fit } => ContentSnapshot::Image {
                source_id: source.id.clone(),
                fit: *fit,
            },
        },
    });
    for (index, child) in node.children.iter().enumerate() {
        collect(child, Some(node.id.as_str()), index, out);
    }
}

fn damage(
    old: &[NodeSnapshot],
    new: &[NodeSnapshot],
    scale: f32,
    width: u32,
    height: u32,
    focus_outset: f32,
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
            let outset = if node.focused { focus_outset } else { 0.0 };
            let r = bounds_rect(node.bounds, scale, width, height, outset);
            result = Some(result.map_or(r, |d: DamageRect| d.union(r)));
        }
    }
    result
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
) -> Result<(), RenderError> {
    let b = node.bounds;
    let surface_key = if node.state.selected {
        "--state-selected-accent"
    } else {
        &node.style_token
    };
    let color = style_color(context.style, surface_key)?;
    if let Some(rect) = Rect::from_xywh(b.x * scale, b.y * scale, b.width * scale, b.height * scale)
    {
        let mut paint = Paint::default();
        paint.set_color_rgba8(color.red, color.green, color.blue, color.alpha);
        pm.fill_rect(rect, &paint, Transform::identity(), None);
        if node.state.focused {
            let ring = style_color(context.style, "--state-focused-ring")?;
            paint.set_color_rgba8(ring.red, ring.green, ring.blue, ring.alpha);
            let width = context
                .style
                .length("--focus-ring-width")
                .expect("typed focus width")
                .pixels
                * scale;
            let offset = context
                .style
                .length("--focus-ring-offset")
                .expect("typed focus offset")
                .pixels
                * scale;
            let stroke = Stroke {
                width,
                ..Stroke::default()
            };
            let outset = offset + width / 2.0;
            if let Some(ring_rect) = Rect::from_xywh(
                rect.x() - outset,
                rect.y() - outset,
                rect.width() + outset * 2.0,
                rect.height() + outset * 2.0,
            ) {
                let path = PathBuilder::from_rect(ring_rect);
                pm.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
        }
    }
    let text_key = if node.state.disabled {
        "--state-disabled-text"
    } else if node.state.focused {
        "--state-focused-text"
    } else {
        "--state-rest-text"
    };
    let text = style_color(context.style, text_key)?;
    match &node.content {
        NodeContent::Label => {
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
        NodeContent::Image { source, fit } => {
            if let Err(note) = draw_image(pm, context.images, source, *fit, b, scale) {
                context.notes.push(note);
            }
        }
    }
    for child in &node.children {
        draw_node(pm, context, child, scale)?;
    }
    Ok(())
}

fn draw_image(
    target: &mut Pixmap,
    cache: &mut ImageCache,
    source: &ImageSource,
    fit: ImageFit,
    bounds: Bounds,
    surface_scale: f32,
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
    for row in top.min(target.height())..bottom {
        let start = (row * target.width() + left.min(target.width())) as usize;
        let end = (row * target.width() + right) as usize;
        mask_data[start..end].fill(255);
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
            .letter_spacing(draw.style.tracking_em * size),
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
        let root = Node::new(
            NodeId::new("root").unwrap(),
            Role::Button,
            label,
            Bounds::new(3.0, 4.0, 250.0, 80.0),
            "--state-rest-surface",
        )
        .with_action(NodeAction::Activate);
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

    #[test]
    fn dusk_is_the_default_and_all_bases_are_selectable() {
        let scene = fixture("Theme");
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
        let scene = fixture("Theme");
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
        assert_eq!(
            unfocused.damage,
            Some(DamageRect {
                x: 15,
                y: 15,
                width: 50,
                height: 40,
            })
        );
        assert_eq!(pixel(&unfocused, 15, 35), [23, 21, 18, 255]);

        let focused_again = rasterizer.render(&focus_scene(true), metrics()).unwrap();
        assert_eq!(focused_again.damage, unfocused.damage);
        assert_eq!(pixel(&focused_again, 15, 35), [243, 223, 174, 255]);
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
        assert_eq!(
            frame.damage,
            Some(DamageRect {
                x: 12,
                y: 6,
                width: 183,
                height: 139,
            })
        );
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
        let node = Node::new(
            NodeId::new("root").unwrap(),
            Role::Button,
            "This label wraps across far more lines than fit",
            bounds,
            "--state-rest-surface",
        )
        .with_action(NodeAction::Activate);
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

    fn tracked_layout(text: &str, width: f32, spacing: f32) -> Vec<(f32, Vec<f32>)> {
        let mut db = tracked_text::fontdb::Database::new();
        db.load_font_data(MANROPE.to_vec());
        let mut fonts = tracked_text::FontSystem::new_with_locale_and_db("en-US".into(), db);
        let mut buffer =
            tracked_text::Buffer::new(&mut fonts, tracked_text::Metrics::new(11.5, 14.375));
        buffer.set_size(&mut fonts, Some(width), Some(200.0));
        buffer.set_text(
            &mut fonts,
            text,
            &tracked_text::Attrs::new()
                .family(tracked_text::Family::Name("Manrope"))
                .weight(tracked_text::Weight(700))
                .letter_spacing(spacing),
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
    fn tracked_run_width_is_the_sum_of_its_tracked_advances() {
        let runs = tracked_layout("EYEBROW", 1_000.0, 11.5 * 0.14);
        assert_eq!(runs.len(), 1);
        let (measured, advances) = &runs[0];
        let advance_sum: f32 = advances.iter().sum();
        assert!(
            (measured - advance_sum).abs() < 0.001,
            "{measured} != {advance_sum}"
        );
    }

    #[test]
    fn tracked_eyebrow_wraps_before_crossing_the_node_clip() {
        let text = "TRACKED EYEBROW";
        let untracked_width = tracked_layout(text, 1_000.0, 0.0)[0].0;
        let tracked_width = tracked_layout(text, 1_000.0, 11.5 * 0.14)[0].0;
        assert!(tracked_width > untracked_width);

        let node_width = (untracked_width + tracked_width) / 2.0;
        assert_eq!(tracked_layout(text, node_width, 0.0).len(), 1);
        let tracked = tracked_layout(text, node_width, 11.5 * 0.14);
        assert!(tracked.len() > 1, "tracking-aware text did not wrap");
        assert!(tracked
            .iter()
            .all(|(line_width, _)| *line_width <= node_width));
    }
}
