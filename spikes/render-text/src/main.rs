use cosmic_text::{fontdb, Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache};
use std::{env, fs, hint::black_box, path::Path, time::Instant};
use tiny_skia::{Color as SkColor, Paint, Pixmap, Rect, Transform};

const W: u32 = 1280;
const H: u32 = 720;

fn fill(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, rgba: u32) {
    let mut p = Paint::default();
    p.set_color_rgba8(
        (rgba >> 24) as u8,
        (rgba >> 16) as u8,
        (rgba >> 8) as u8,
        rgba as u8,
    );
    if let Some(r) = Rect::from_xywh(x, y, w, h) {
        pm.fill_rect(r, &p, Transform::identity(), None);
    }
}

fn text(
    pm: &mut Pixmap,
    fonts: &mut FontSystem,
    cache: &mut SwashCache,
    s: &str,
    x: f32,
    y: f32,
    width: f32,
    size: f32,
    family: Family<'_>,
    color: u32,
) {
    let mut b = Buffer::new(fonts, Metrics::new(size, size * 1.25));
    b.set_size(fonts, Some(width), None);
    b.set_text(fonts, s, Attrs::new().family(family), Shaping::Advanced);
    b.shape_until_scroll(fonts, false);
    b.draw(
        fonts,
        cache,
        Color::rgba(
            (color >> 24) as u8,
            (color >> 16) as u8,
            (color >> 8) as u8,
            color as u8,
        ),
        |gx, gy, gw, gh, c| {
            let a = c.a() as u32;
            let src = [c.r() as u32, c.g() as u32, c.b() as u32];
            for yy in 0..gh as i32 {
                for xx in 0..gw as i32 {
                    let px = gx + xx + x as i32;
                    let py = gy + yy + y as i32;
                    if px < 0 || py < 0 || px >= pm.width() as i32 || py >= pm.height() as i32 {
                        continue;
                    }
                    let idx = py as usize * pm.width() as usize + px as usize;
                    let d = &mut pm.pixels_mut()[idx];
                    let old = d.demultiply();
                    let inv = 255 - a;
                    *d = tiny_skia::PremultipliedColorU8::from_rgba(
                        ((src[0] * a + old.red() as u32 * inv) / 255) as u8,
                        ((src[1] * a + old.green() as u32 * inv) / 255) as u8,
                        ((src[2] * a + old.blue() as u32 * inv) / 255) as u8,
                        255,
                    )
                    .unwrap();
                }
            }
        },
    );
}

fn render(path: &Path, w: u32, h: u32, scale: f32) {
    // Start empty: FontSystem::new() discovers host fonts, which would make both
    // fallback behavior and the committed PNGs depend on the machine running this.
    let mut db = fontdb::Database::new();
    for p in [
        "fonts/manrope/Manrope[wght].ttf",
        "fonts/fraunces/Fraunces[SOFT,WONK,opsz,wght].ttf",
    ] {
        db.load_font_data(fs::read(p).expect("vendored font"));
    }
    let mut fonts = FontSystem::new_with_locale_and_db("en-US".into(), db);
    let mut cache = SwashCache::new();
    let mut pm = Pixmap::new(w, h).unwrap();
    pm.fill(SkColor::from_rgba8(13, 17, 23, 255));
    let margin = w as f32 * 0.055;
    let usable = w as f32 - margin * 2.0;
    text(
        &mut pm,
        &mut fonts,
        &mut cache,
        "POCKETFORGE  /  HOME",
        margin,
        30.0,
        usable,
        15.0 * scale,
        Family::Name("Manrope"),
        0x9aa7b8ff,
    );
    text(
        &mut pm,
        &mut fonts,
        &mut cache,
        "Pick up where you left off.",
        margin,
        72.0,
        usable,
        34.0 * scale,
        Family::Name("Fraunces"),
        0xf4eadcff,
    );
    text(
        &mut pm,
        &mut fonts,
        &mut cache,
        "Café • العربية • हिन्दी • 日本語 • 한글 • emoji ✨",
        margin,
        125.0,
        usable,
        18.0 * scale,
        Family::Name("Manrope"),
        0xc7d1ddff,
    );
    let gap = 14.0;
    let card_w = (usable - gap * 2.0) / 3.0;
    let card_h = (150.0 * scale).min(h as f32 * 0.30);
    for i in 0..3 {
        let x = margin + i as f32 * (card_w + gap);
        fill(
            &mut pm,
            x,
            190.0,
            card_w,
            card_h,
            if i == 0 { 0x24415fff } else { 0x1a2430ff },
        );
    }
    fill(
        &mut pm,
        margin - 3.0,
        187.0,
        card_w + 6.0,
        card_h + 6.0,
        0x7dc4ffff,
    );
    fill(&mut pm, margin, 190.0, card_w, card_h, 0x24415fff);
    text(
        &mut pm,
        &mut fonts,
        &mut cache,
        "CONTINUE\nSea of Stars",
        margin + 18.0,
        210.0,
        card_w - 36.0,
        18.0 * scale,
        Family::Name("Manrope"),
        0xffffffff,
    );
    text(
        &mut pm,
        &mut fonts,
        &mut cache,
        "LIBRARY\n42 games",
        margin + card_w + gap + 18.0,
        210.0,
        card_w - 36.0,
        18.0 * scale,
        Family::Name("Manrope"),
        0xffffffff,
    );
    text(
        &mut pm,
        &mut fonts,
        &mut cache,
        "SETTINGS\nReady",
        margin + 2.0 * (card_w + gap) + 18.0,
        210.0,
        card_w - 36.0,
        18.0 * scale,
        Family::Name("Manrope"),
        0xffffffff,
    );
    let states = [
        "default", "focused", "pressed", "disabled", "loading", "error", "empty",
    ];
    let sy = (190.0 + card_h + 45.0).min(h as f32 - 120.0);
    for (i, s) in states.iter().enumerate() {
        let x = margin + i as f32 * (usable / 7.0);
        fill(
            &mut pm,
            x,
            sy,
            usable / 7.0 - 7.0,
            48.0,
            if *s == "error" {
                0x612c32ff
            } else if *s == "disabled" {
                0x20252bff
            } else {
                0x283646ff
            },
        );
        text(
            &mut pm,
            &mut fonts,
            &mut cache,
            s,
            x + 6.0,
            sy + 13.0,
            usable / 7.0 - 12.0,
            11.0 * scale,
            Family::Name("Manrope"),
            0xe9edf2ff,
        );
    }
    pm.save_png(path).unwrap();
}

fn bench() {
    let n = (W * H) as usize;
    let a = vec![0x18324affu32; n];
    let b = vec![0xd18a62ffu32; n];
    let mut out = vec![0u32; n];
    for alpha in [0u32, 64, 128, 192, 255] {
        let t = Instant::now();
        for _ in 0..20 {
            for i in 0..n {
                let x = a[i];
                let y = b[i];
                let inv = 255 - alpha;
                let mut v = 255;
                for shift in [8, 16, 24] {
                    v |= ((((x >> shift) & 255) * inv + ((y >> shift) & 255) * alpha) / 255)
                        << shift;
                }
                out[i] = v;
            }
            black_box(&out);
        }
        println!(
            "D1 alpha={alpha} ms_frame={:.3}",
            t.elapsed().as_secs_f64() * 1000.0 / 20.0
        );
    }
    let mut shadow = vec![0u32; n];
    let mut page0 = vec![0u32; n];
    let mut page1 = vec![0u32; n];
    for band in [16usize, 64, 160, 320, 720] {
        let t = Instant::now();
        for f in 0..120 {
            let y = (f * 13) % H as usize;
            let end = (y + band).min(H as usize);
            shadow[y * W as usize..end * W as usize].fill(f as u32);
            let p = if f % 2 == 0 { &mut page0 } else { &mut page1 };
            let from = y.saturating_sub(band) * W as usize;
            let to = end * W as usize;
            p[from..to].copy_from_slice(&shadow[from..to]);
            black_box(&p);
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0 / 120.0;
        println!(
            "D2 band_px={band} accumulated_px_max={} ms_frame={ms:.3} fps={:.1}",
            (band * 2).min(H as usize),
            1000.0 / ms
        );
    }
    if let Ok(s) = fs::read_to_string("/proc/self/status") {
        if let Some(l) = s.lines().find(|x| x.starts_with("VmHWM:")) {
            println!("RSS {l}");
        }
    }
}

fn main() {
    env::set_current_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
    let a: Vec<_> = env::args().collect();
    if a.get(1).map(String::as_str) == Some("bench") {
        bench();
        return;
    }
    let out = a
        .get(1)
        .map(String::as_str)
        .unwrap_or("evidence/home-1280x720.png");
    let w = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(W);
    let h = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(H);
    let scale = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(1.0);
    render(Path::new(out), w, h, scale);
}
