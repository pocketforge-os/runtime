//! Deterministic, framework-neutral text measurement and RGBA rasterization.

use cosmic_text_tracking as text;
use tiny_skia::Pixmap;

pub type FontSystem = text::FontSystem;
pub type SwashCache = text::SwashCache;
pub type Rgba8 = [u8; 4];

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedTextStyle {
    pub family: String,
    pub size_px: f32,
    pub weight: u16,
    pub line_height: f32,
    pub tracking_em: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextAlign {
    Start,
    Center,
}

/// An owned, repeatable font recipe. Every produced system loads exactly these bytes in order.
#[derive(Clone, Debug)]
pub struct FontSet {
    font_datas: Vec<Vec<u8>>,
    locale: String,
}

impl FontSet {
    pub fn new(font_datas: &[&[u8]], locale: &str) -> Self {
        Self {
            font_datas: font_datas.iter().map(|data| data.to_vec()).collect(),
            locale: locale.to_owned(),
        }
    }

    /// Construct the single font-system recipe shared by measurement and painting.
    pub fn font_system(&self) -> FontSystem {
        let mut db = text::fontdb::Database::new();
        // Deliberately never load OS fonts: repository bytes are the deterministic universe.
        for data in &self.font_datas {
            db.load_font_data(data.clone());
        }
        FontSystem::new_with_locale_and_db(self.locale.clone(), db)
    }
}

#[cfg(feature = "default-fonts")]
pub fn pocketforge_default_fonts() -> FontSet {
    const MANROPE: &[u8] = include_bytes!("../fonts/Manrope[wght].ttf");
    const FRAUNCES: &[u8] = include_bytes!("../fonts/Fraunces[SOFT,WONK,opsz,wght].ttf");
    const CJK: &[u8] = include_bytes!("../fonts/NotoSansCJK-Regular.ttc");
    FontSet::new(&[MANROPE, FRAUNCES, CJK], "en-US")
}

fn metrics(style: &ResolvedTextStyle, scale: f32) -> (f32, f32) {
    let size = style.size_px * scale;
    (size, size * style.line_height)
}

fn attrs(style: &ResolvedTextStyle) -> text::Attrs<'_> {
    text::Attrs::new()
        .family(text::Family::Name(&style.family))
        .weight(text::Weight(style.weight))
        .letter_spacing(style.tracking_em)
}

pub fn measure(
    fonts: &FontSet,
    style: &ResolvedTextStyle,
    text_value: &str,
    scale: f32,
    max_width: Option<f32>,
) -> (f32, f32) {
    let mut font_system = fonts.font_system();
    let (size, line_height) = metrics(style, scale);
    let mut buffer = text::Buffer::new(&mut font_system, text::Metrics::new(size, line_height));
    buffer.set_size(&mut font_system, max_width, None);
    buffer.set_text(
        &mut font_system,
        text_value,
        &attrs(style),
        text::Shaping::Advanced,
        None,
    );
    buffer.shape_until_scroll(&mut font_system, false);
    buffer
        .layout_runs()
        .fold((0.0_f32, 0.0_f32), |(w, h), run| {
            (w.max(run.line_w), h.max(run.line_top + run.line_height))
        })
}

pub struct TextDraw<'a> {
    pub text: &'a str,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub style: &'a ResolvedTextStyle,
    pub scale: f32,
    pub align: TextAlign,
    pub clip: (f32, f32, f32, f32),
    pub color: Rgba8,
}

pub fn draw_text(
    pixmap: &mut Pixmap,
    fonts: &mut FontSystem,
    glyphs: &mut SwashCache,
    draw: TextDraw<'_>,
) {
    let (size, line_height) = metrics(draw.style, draw.scale);
    let mut buffer = text::Buffer::new(fonts, text::Metrics::new(size, line_height));
    buffer.set_size(fonts, Some(draw.width), Some(draw.height));
    buffer.set_text(
        fonts,
        draw.text,
        &attrs(draw.style),
        text::Shaping::Advanced,
        None,
    );
    if draw.align == TextAlign::Center {
        for line in &mut buffer.lines {
            line.set_align(Some(text::Align::Center));
        }
    }
    buffer.shape_until_scroll(fonts, false);
    let vertical_offset = if draw.align == TextAlign::Center {
        let content_height = buffer
            .layout_runs()
            .map(|run| run.line_top + run.line_height)
            .fold(0.0_f32, f32::max);
        ((draw.height - content_height) / 2.0).max(0.0)
    } else {
        0.0
    };
    let default_color =
        text::Color::rgba(draw.color[0], draw.color[1], draw.color[2], draw.color[3]);
    for run in buffer.layout_runs() {
        for glyph in run.glyphs {
            let physical = glyph.physical((0.0, 0.0), 1.0);
            let color = glyph.color_opt.unwrap_or(default_color);
            glyphs.with_pixels(fonts, physical.cache_key, color, |x, y, color| {
                blend_text_pixel_rgba(
                    pixmap,
                    &draw,
                    physical.x + x,
                    (run.line_y + vertical_offset) as i32 + physical.y + y,
                    [color.r(), color.g(), color.b(), color.a()],
                );
            });
        }
    }
}

fn blend_text_pixel_rgba(pixmap: &mut Pixmap, draw: &TextDraw<'_>, gx: i32, gy: i32, color: Rgba8) {
    let px = gx + draw.x as i32;
    let py = gy + draw.y as i32;
    if px < draw.clip.0.floor() as i32
        || py < draw.clip.1.floor() as i32
        || px >= (draw.clip.0 + draw.clip.2).ceil() as i32
        || py >= (draw.clip.1 + draw.clip.3).ceil() as i32
        || px < 0
        || py < 0
        || px >= pixmap.width() as i32
        || py >= pixmap.height() as i32
    {
        return;
    }
    let alpha = color[3] as u32;
    let pixmap_width = pixmap.width() as usize;
    let dst = &mut pixmap.pixels_mut()[py as usize * pixmap_width + px as usize];
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
