//! Deterministic, framework-neutral text measurement and RGBA rasterization.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

/// An ordered, repeatable font recipe. Every produced system loads exactly these sources in
/// exactly this order, and the order is part of the deterministic result: fallback selection
/// between same-weight faces follows load order.
///
/// Disk sources are memory-mapped once at construction and the mapping is shared by every
/// [`FontSystem`] built from this set and by every clone of it, so neither measurement nor
/// painting ever copies font bytes onto the heap.
pub struct FontSet {
    sources: Vec<text::fontdb::Source>,
    locale: String,
    shared_system: RefCell<Option<FontSystem>>,
}

impl Clone for FontSet {
    fn clone(&self) -> Self {
        Self {
            sources: self.sources.clone(),
            locale: self.locale.clone(),
            shared_system: RefCell::new(None),
        }
    }
}

impl std::fmt::Debug for FontSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FontSet")
            .field("sources", &self.sources)
            .field("locale", &self.locale)
            .finish_non_exhaustive()
    }
}

impl FontSet {
    /// Construct a set owning the given font bytes. The bytes are copied once here and then
    /// shared by every system built from the set; prefer [`FontSet::from_files`] for fonts that
    /// live on disk so the bytes stay in the shared, reclaimable file mapping.
    pub fn new(font_datas: &[&[u8]], locale: &str) -> Self {
        Self::from_sources(
            font_datas
                .iter()
                .map(|data| text::fontdb::Source::Binary(Arc::new(data.to_vec()))),
            locale,
        )
    }

    /// Construct a set memory-mapping the given font files, in the given order.
    ///
    /// The mapping is created once and shared across every system this set (and its clones)
    /// builds; measurement performs no font-sized allocation.
    pub fn from_files<P: AsRef<Path>>(
        paths: impl IntoIterator<Item = P>,
        locale: &str,
    ) -> std::io::Result<Self> {
        let mut sources = Vec::new();
        for path in paths {
            sources.push(map_font_file(path.as_ref())?);
        }
        Ok(Self::from_sources(sources, locale))
    }

    fn from_sources(sources: impl IntoIterator<Item = text::fontdb::Source>, locale: &str) -> Self {
        Self {
            sources: sources.into_iter().collect(),
            locale: locale.to_owned(),
            shared_system: RefCell::new(None),
        }
    }

    /// On-disk paths of the file-backed fonts (`None` for owned-bytes fonts), in load order.
    pub fn source_paths(&self) -> Vec<Option<PathBuf>> {
        self.sources.iter().map(font_source_path).collect()
    }

    /// Construct the single font-system recipe shared by measurement and painting.
    pub fn font_system(&self) -> FontSystem {
        let mut db = text::fontdb::Database::new();
        // Deliberately never load OS fonts: the recorded sources are the deterministic universe.
        // Cloning a source is an Arc clone: file mappings and owned bytes are shared, not copied.
        for source in &self.sources {
            db.load_font_source(source.clone());
        }
        FontSystem::new_with_locale_and_db(self.locale.clone(), db)
    }

    /// The system backing [`measure`]: built at most once per set and reused across every
    /// measurement, so shaping never rebuilds the font database or re-touches font bytes.
    fn measure_system(&self) -> std::cell::RefMut<'_, FontSystem> {
        if self.shared_system.borrow().is_none() {
            *self.shared_system.borrow_mut() = Some(self.font_system());
        }
        std::cell::RefMut::map(self.shared_system.borrow_mut(), |slot| {
            slot.as_mut().expect("shared system was just populated")
        })
    }
}

fn map_font_file(path: &Path) -> std::io::Result<text::fontdb::Source> {
    let file = std::fs::File::open(path)?;
    // SAFETY: font files are read-only assets (repository checkout or system font directory);
    // no process mutates them while mapped. This is the same contract fontdb itself documents
    // for file-backed sources.
    let map = unsafe { memmap2::MmapOptions::new().map(&file) }?;
    Ok(text::fontdb::Source::SharedFile(
        path.to_path_buf(),
        Arc::new(map),
    ))
}

fn font_source_path(source: &text::fontdb::Source) -> Option<PathBuf> {
    match source {
        text::fontdb::Source::Binary(_) => None,
        text::fontdb::Source::File(path) => Some(path.clone()),
        text::fontdb::Source::SharedFile(path, _) => Some(path.clone()),
    }
}

/// Load order below is part of the deterministic recipe: same-weight fallback ties resolve by
/// insertion order inside cosmic-text, so the canonical faces come first and any additional
/// installed faces follow in sorted order.
///
/// The CJK collection is the third canonical face — strictly after the two Latin faces, so
/// same-weight Latin tie order is unchanged — and is the guaranteed display fallback for CJK
/// user data (app titles, SSIDs, Bluetooth names) in every UI language. Input-method language
/// packs may install additional faces later (design/input-layer #l10n L2); those load as extras.
#[cfg(feature = "default-fonts")]
const CANONICAL_FONT_ORDER: [&str; 3] = [
    "Manrope[wght].ttf",
    "Fraunces[SOFT,WONK,opsz,wght].ttf",
    "NotoSansCJK-Regular.ttc",
];

/// Where the device image installs the production fonts (image repo
/// `scripts/install-shell-fonts.sh`).
#[cfg(feature = "default-fonts")]
const INSTALLED_FONTS_DIR: &str = "/usr/share/fonts/pocketforge";

/// The shared production font set, memory-mapped from disk through one fallback chain.
///
/// Font directory resolution, first match wins:
/// 1. `$PF_FONTS_DIR` (explicit override),
/// 2. `/usr/share/fonts/pocketforge` (device image; ships the canonical faces there),
/// 3. this crate's `fonts/` directory (repository checkout: development and CI).
///
/// Font bytes are never embedded in binaries. The selected directory must contain every
/// canonical face (the two Latin faces and the Noto Sans CJK display fallback); a missing,
/// unreadable, or partial directory (one missing any canonical face, even if it holds other
/// fonts) is a fatal configuration error and panics naming the path and the missing faces,
/// rather than letting cosmic-text silently substitute a different face and change metrics
/// or drop guaranteed CJK coverage.
#[cfg(feature = "default-fonts")]
pub fn pocketforge_default_fonts() -> FontSet {
    font_set_from_dir(&default_fonts_dir())
}

#[cfg(feature = "default-fonts")]
fn font_set_from_dir(dir: &Path) -> FontSet {
    let files = default_font_files(dir);
    FontSet::from_files(&files, "en-US").unwrap_or_else(|error| {
        panic!("failed to memory-map fonts from {}: {error}", dir.display())
    })
}

#[cfg(feature = "default-fonts")]
fn default_fonts_dir() -> PathBuf {
    fonts_dir_with_installed(Path::new(INSTALLED_FONTS_DIR))
}

/// The resolution order of [`pocketforge_default_fonts`], with the installed-directory path
/// as a parameter so the branch is testable hermetically (no root-owned
/// `/usr/share/fonts/pocketforge` fixture needed).
#[cfg(feature = "default-fonts")]
fn fonts_dir_with_installed(installed: &Path) -> PathBuf {
    if let Some(dir) = std::env::var_os("PF_FONTS_DIR") {
        return PathBuf::from(dir);
    }
    if installed.is_dir() {
        return installed.to_path_buf();
    }
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/fonts"))
}

#[cfg(feature = "default-fonts")]
fn default_font_files(dir: &Path) -> Vec<PathBuf> {
    let mut present: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|entry| entry.expect("font directory entry readable").path())
            .filter(|path| is_font_file(path))
            .collect(),
        Err(error) => panic!("font directory {} is unreadable: {error}", dir.display()),
    };
    present.sort();
    // Fail closed unless EVERY canonical face is present in the selected directory: a partial
    // or stale directory — including a Latin-only one that drops the guaranteed CJK fallback —
    // must not load, or cosmic-text silently substitutes the remaining faces, changes layout
    // metrics and renders CJK user data as missing glyphs. Validate before loading extras and
    // name every missing face.
    let missing: Vec<&str> = CANONICAL_FONT_ORDER
        .iter()
        .filter(|name| !present.contains(&dir.join(*name)))
        .copied()
        .collect();
    if !missing.is_empty() {
        panic!(
            "font directory {} is missing canonical font(s) {}; refusing to substitute other faces",
            dir.display(),
            missing.join(", ")
        );
    }
    let mut ordered: Vec<PathBuf> = CANONICAL_FONT_ORDER
        .iter()
        .map(|name| dir.join(name))
        .collect();
    let extra: Vec<PathBuf> = present
        .into_iter()
        .filter(|path| !ordered.contains(path))
        .collect();
    ordered.extend(extra);
    ordered
}

#[cfg(feature = "default-fonts")]
fn is_font_file(path: &Path) -> bool {
    path.extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "ttf" | "otf" | "ttc" | "otc"
            )
        })
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
    let mut font_system = fonts.measure_system();
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

#[cfg(all(test, feature = "default-fonts"))]
mod default_font_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync>;

    struct HookGuard(Option<PanicHook>);
    impl Drop for HookGuard {
        fn drop(&mut self) {
            if let Some(hook) = self.0.take() {
                std::panic::set_hook(hook);
            }
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "pf-text-raster-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).expect("create temp font dir");
        dir
    }

    fn write_names(dir: &Path, names: &[&str]) {
        for name in names {
            std::fs::write(dir.join(name), b"").expect("write font placeholder");
        }
    }

    // Removes each created file then the directory: no recursive delete, no rm -rf.
    fn cleanup(dir: &Path) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let _ = std::fs::remove_dir(dir);
    }

    // Resolve `dir`, returning Ok(files) or Err(panic message). The expected-panic
    // message is captured (and the default hook silenced) so a fail-closed case is
    // proven to reject for the RIGHT reason, not panic incidentally.
    fn resolve_capturing_panic(
        dir: &Path,
        captured: &Arc<Mutex<String>>,
    ) -> Result<Vec<PathBuf>, ()> {
        captured.lock().expect("capture lock").clear();
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| default_font_files(dir)))
            .map_err(|_| ())
    }

    fn names_of(paths: Vec<PathBuf>) -> Vec<String> {
        paths
            .iter()
            .map(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .expect("font file name is utf-8")
                    .to_owned()
            })
            .collect()
    }

    // One resolver-case result recorded under the capture hook. Assertions run
    // only AFTER the original panic hook is restored: an assertion that fires
    // while the capturing hook is installed aborts the test process with a
    // "non-unwinding panic" instead of reporting a failure, hiding exactly the
    // evidence a negative control exists to show.
    struct Case {
        tag: &'static str,
        resolved: Result<Vec<String>, ()>,
        message: String,
    }

    const ALL_CANONICAL: [&str; 3] = [
        "Manrope[wght].ttf",
        "Fraunces[SOFT,WONK,opsz,wght].ttf",
        "NotoSansCJK-Regular.ttc",
    ];

    #[test]
    fn default_font_files_requires_all_canonical_faces() {
        let mut cases: Vec<Case> = Vec::new();

        // --- capture phase: positive and negative controls in one invocation ---
        {
            let captured = Arc::new(Mutex::new(String::new()));
            let captured_hook = captured.clone();
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                let text = info
                    .payload()
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| {
                        info.payload()
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_owned())
                    })
                    .unwrap_or_default();
                *captured_hook.lock().expect("capture lock") = text;
            }));
            let _guard = HookGuard(Some(prev));

            // Positive control: all canonical faces present (plus another extra font
            // and a non-font license file) resolve canonical-first, extras sorted,
            // non-fonts ignored.
            let ok = temp_dir("ok");
            write_names(
                &ok,
                &[
                    "Manrope[wght].ttf",
                    "Fraunces[SOFT,WONK,opsz,wght].ttf",
                    "NotoSansCJK-Regular.ttc",
                    "Zilla.ttf",
                    "Manrope-OFL.txt",
                ],
            );
            let resolved = resolve_capturing_panic(&ok, &captured);
            cases.push(Case {
                tag: "positive",
                resolved: resolved.map(names_of),
                message: captured.lock().expect("capture lock").clone(),
            });
            cleanup(&ok);

            // Negative controls: each partial/stale/empty directory must fail closed
            // AND name the missing canonical face(s), so cosmic-text never silently
            // substitutes. The missing-cjk case is the tsp-ihne1.12 reopen
            // regression: a Latin-only directory drops the guaranteed CJK fallback
            // for user data (app titles, SSIDs, Bluetooth names) and must be refused
            // like any other partial set.
            for (tag, names) in [
                ("empty", &[][..]),
                ("only-other", &["DejaVuSans.ttf"][..]),
                (
                    "missing-fraunces",
                    &["Manrope[wght].ttf", "NotoSansCJK-Regular.ttc"][..],
                ),
                (
                    "missing-manrope",
                    &[
                        "Fraunces[SOFT,WONK,opsz,wght].ttf",
                        "NotoSansCJK-Regular.ttc",
                    ][..],
                ),
                (
                    "missing-cjk",
                    &["Manrope[wght].ttf", "Fraunces[SOFT,WONK,opsz,wght].ttf"][..],
                ),
                (
                    "cjk-rescues-neither",
                    &["NotoSansCJK-Regular.ttc", "Manrope[wght].ttf"][..],
                ),
            ] {
                let dir = temp_dir(tag);
                write_names(&dir, names);
                let outcome = resolve_capturing_panic(&dir, &captured);
                cases.push(Case {
                    tag,
                    resolved: outcome.map(names_of),
                    message: captured.lock().expect("capture lock").clone(),
                });
                cleanup(&dir);
            }
        }

        // --- assert phase: the original hook is restored, failures report ---
        let case = |tag: &str| {
            cases
                .iter()
                .find(|case| case.tag == tag)
                .unwrap_or_else(|| panic!("case {tag} did not run"))
        };

        assert_eq!(
            case("positive").resolved,
            Ok([
                "Manrope[wght].ttf",
                "Fraunces[SOFT,WONK,opsz,wght].ttf",
                "NotoSansCJK-Regular.ttc",
                "Zilla.ttf",
            ]
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<String>>()),
            "positive control: canonical faces must lead in order, extras follow \
             sorted, non-fonts ignored; panic message: {}",
            case("positive").message
        );

        for (tag, expected_missing) in [
            ("empty", &ALL_CANONICAL[..]),
            ("only-other", &ALL_CANONICAL[..]),
            (
                "missing-fraunces",
                &["Fraunces[SOFT,WONK,opsz,wght].ttf"][..],
            ),
            ("missing-manrope", &["Manrope[wght].ttf"][..]),
            // The reopen regression: Latin-only is a partial, refused set.
            ("missing-cjk", &["NotoSansCJK-Regular.ttc"][..]),
            (
                "cjk-rescues-neither",
                &["Fraunces[SOFT,WONK,opsz,wght].ttf"][..],
            ),
        ] {
            let case = case(tag);
            assert!(
                case.resolved.is_err(),
                "case {tag}: a partial font directory must fail closed, not substitute \
                 another face; it resolved {:?}",
                case.resolved
            );
            assert!(
                case.message.contains("missing canonical font"),
                "case {tag}: rejection must state the missing-canonical reason; got: {}",
                case.message
            );
            for name in expected_missing {
                assert!(
                    case.message.contains(name),
                    "case {tag}: rejection must name the missing face {name}; got: {}",
                    case.message
                );
            }
        }
    }

    fn copy_real_faces(dir: &Path) {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("fonts");
        for face in ALL_CANONICAL {
            std::fs::copy(src.join(face), dir.join(face))
                .unwrap_or_else(|error| panic!("copy {face} into fixture: {error}"));
        }
    }

    /// The CJK guarantee, at both levels: every non-ASCII cluster of the sample
    /// shapes to a real (non-`.notdef`) glyph of the Noto Sans CJK face, and the
    /// production draw path puts ink on the pixmap.
    fn cjk_renders_with_real_glyphs(set: &FontSet, branch: &str) {
        let sample = "\u{518d}\u{958b} \u{65e5}\u{672c}\u{8a9e}";

        let mut system = set.font_system();
        let mut buffer = text::Buffer::new(&mut system, text::Metrics::new(16.0, 24.0));
        buffer.set_size(&mut system, Some(240.0), None);
        buffer.set_text(
            &mut system,
            sample,
            &text::Attrs::new()
                .family(text::Family::Name("Manrope"))
                .weight(text::Weight(400)),
            text::Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(&mut system, false);
        let mut cjk_glyphs = 0usize;
        for run in buffer.layout_runs() {
            for glyph in run.glyphs {
                let cluster = &sample[glyph.start..glyph.end];
                assert_ne!(
                    glyph.glyph_id, 0,
                    "{branch}: cluster {cluster:?} shaped to .notdef"
                );
                if !cluster.is_ascii() {
                    let face = system
                        .db()
                        .face(glyph.font_id)
                        .unwrap_or_else(|| panic!("{branch}: glyph font id resolves"));
                    let path = match &face.source {
                        text::fontdb::Source::SharedFile(path, _)
                        | text::fontdb::Source::File(path) => path.clone(),
                        text::fontdb::Source::Binary(_) => {
                            panic!("{branch}: CJK glyph came from an embedded source")
                        }
                    };
                    assert_eq!(
                        path.file_name().and_then(|name| name.to_str()),
                        Some("NotoSansCJK-Regular.ttc"),
                        "{branch}: CJK cluster {cluster:?} fell back to {}",
                        path.display()
                    );
                    cjk_glyphs += 1;
                }
            }
        }
        assert!(
            cjk_glyphs >= 5,
            "{branch}: expected the 5 CJK clusters shaped from the CJK face, saw {cjk_glyphs}"
        );

        let mut system = set.font_system();
        let mut cache = SwashCache::new();
        let mut pixmap = Pixmap::new(240, 48).expect("pixmap allocates");
        let style = ResolvedTextStyle {
            family: "Manrope".to_owned(),
            size_px: 16.0,
            weight: 400,
            line_height: 1.5,
            tracking_em: 0.0,
        };
        draw_text(
            &mut pixmap,
            &mut system,
            &mut cache,
            TextDraw {
                text: sample,
                x: 4.0,
                y: 4.0,
                width: 232.0,
                height: 40.0,
                style: &style,
                scale: 1.0,
                align: TextAlign::Start,
                clip: (0.0, 0.0, 240.0, 48.0),
                color: [255, 255, 255, 255],
            },
        );
        let ink = pixmap
            .pixels()
            .iter()
            .filter(|pixel| pixel.alpha() > 0)
            .count();
        assert!(
            ink > 100,
            "{branch}: CJK raster carries no ink ({ink} opaque pixels)"
        );
    }

    // Run `build`, returning the panic message it was refused with (empty if it
    // did not panic). Only ever called while the capturing hook is installed.
    fn capture_panic(captured: &Arc<Mutex<String>>, build: impl FnOnce()) -> String {
        captured.lock().expect("capture lock").clear();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(build));
        captured.lock().expect("capture lock").clone()
    }

    #[test]
    fn installed_and_override_branches_guarantee_cjk_fallback() {
        // The reopen guarantee, per resolver branch, positive and negative in one
        // invocation: a complete directory renders CJK with real glyphs from the
        // CJK face; the SAME directory minus the CJK face is refused for the
        // missing-canonical reason. The installed-directory branch is exercised
        // through fonts_dir_with_installed() with a fixture path, because the
        // production constant /usr/share/fonts/pocketforge cannot be created
        // hermetically without root; the override branch goes end to end through
        // the public pocketforge_default_fonts().
        let installed = temp_dir("branch-installed");
        let over = temp_dir("branch-override");
        copy_real_faces(&installed);
        copy_real_faces(&over);

        let installed_dir;
        let installed_set;
        let installed_latin_only_message;
        let override_dir;
        let override_set;
        let override_latin_only_message;

        // --- capture phase: no assertions here, intentional panics only inside
        // catch_unwind (an assertion under the swapped hook aborts the process).
        {
            let captured = Arc::new(Mutex::new(String::new()));
            let captured_hook = captured.clone();
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                let text = info
                    .payload()
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| {
                        info.payload()
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_owned())
                    })
                    .unwrap_or_default();
                *captured_hook.lock().expect("capture lock") = text;
            }));
            let _guard = HookGuard(Some(prev));

            // Branch 1: installed-directory resolution.
            std::env::remove_var("PF_FONTS_DIR");
            installed_dir = fonts_dir_with_installed(&installed);
            installed_set = font_set_from_dir(&installed_dir);
            std::fs::remove_file(installed.join("NotoSansCJK-Regular.ttc"))
                .expect("remove CJK face from installed fixture");
            installed_latin_only_message = capture_panic(&captured, || {
                let _ = font_set_from_dir(&installed_dir);
            });

            // Branch 2: $PF_FONTS_DIR override, through the public entry point.
            std::env::set_var("PF_FONTS_DIR", &over);
            override_dir = default_fonts_dir();
            override_set = pocketforge_default_fonts();
            std::fs::remove_file(over.join("NotoSansCJK-Regular.ttc"))
                .expect("remove CJK face from override fixture");
            override_latin_only_message = capture_panic(&captured, || {
                let _ = pocketforge_default_fonts();
            });
            std::env::remove_var("PF_FONTS_DIR");
        }

        // --- assert phase: the original hook is restored, failures report ---
        assert_eq!(
            installed_dir, installed,
            "installed branch must select the installed directory when no override is set"
        );
        assert_eq!(
            override_dir, over,
            "override branch must select $PF_FONTS_DIR over everything else"
        );
        if !Path::new(INSTALLED_FONTS_DIR).is_dir() {
            assert_eq!(
                default_fonts_dir(),
                Path::new(env!("CARGO_MANIFEST_DIR")).join("fonts"),
                "with no override and no installed directory, resolution falls back to the crate fonts"
            );
        }

        cjk_renders_with_real_glyphs(&installed_set, "installed");
        cjk_renders_with_real_glyphs(&override_set, "override");

        for (branch, message) in [
            ("installed", &installed_latin_only_message),
            ("override", &override_latin_only_message),
        ] {
            assert!(
                message.contains("missing canonical font"),
                "{branch}: a CJK-less directory must be refused for the missing-canonical \
                 reason; got: {message}"
            );
            assert!(
                message.contains("NotoSansCJK-Regular.ttc"),
                "{branch}: the refusal must name the missing CJK face; got: {message}"
            );
        }

        cleanup(&installed);
        cleanup(&over);
    }
}
