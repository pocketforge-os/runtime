//! Loading, validation, resolution, fallback, and CSS generation for data-only themes.
//!
//! The embedded `quiet-console` package is synced from
//! `pocketforge-os/design/theme-quiet-console/package`; `vendor/SOURCE.sha256`
//! records the source package digest and `scripts/check-flagship-sync.sh` compares
//! it with a design checkout when one is available.

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

const MANIFEST: &str = "manifest.json";
const TOKENS: &str = "tokens.json";
/// Base-scoped tokens whose resolved values are consumed by native renderers.
pub const STYLE_TOKENS: &[&str] = &[
    "--color-surface-canvas",
    "--color-surface-raised",
    "--color-surface-overlay",
    "--color-surface-sunken",
    "--color-surface-scrim",
    "--color-text-primary",
    "--color-text-secondary",
    "--color-text-muted",
    "--color-text-inverse",
    "--color-border-hairline",
    "--color-border-strong",
    "--color-focus-ring",
    "--color-focus-glow",
    "--focus-ring-width",
    "--focus-ring-offset",
    "--state-rest-surface",
    "--state-rest-text",
    "--state-focused-ring",
    "--state-focused-text",
    "--state-selected-accent",
    "--state-pressed-shift",
    "--state-disabled-text",
    "--state-disabled-border",
    "--state-unavailable-text",
    "--state-unavailable-veil",
    "--state-destructive-accent",
    "--color-status-ready",
    "--color-status-attention",
    "--color-status-stopped",
    "--deco-plate-a-bg",
    "--deco-plate-a-fg",
    "--deco-plate-b-bg",
    "--deco-plate-b-fg",
    "--deco-plate-c-bg",
    "--deco-plate-c-fg",
    "--deco-plate-d-bg",
    "--deco-plate-d-fg",
    "--deco-plate-e-bg",
    "--deco-plate-e-fg",
    "--deco-plate-f-bg",
    "--deco-plate-f-fg",
];
const BASE_TOKENS: &[&str] = &[
    "--color-surface-canvas",
    "--color-surface-raised",
    "--color-surface-overlay",
    "--color-surface-sunken",
    "--color-surface-scrim",
    "--color-text-primary",
    "--color-text-secondary",
    "--color-text-muted",
    "--color-text-inverse",
    "--color-border-hairline",
    "--color-border-strong",
    "--color-focus-ring",
    "--color-focus-glow",
    "--focus-ring-width",
    "--focus-ring-offset",
    "--state-rest-surface",
    "--state-rest-text",
    "--state-focused-ring",
    "--state-focused-text",
    "--state-selected-accent",
    "--state-pressed-shift",
    "--state-disabled-text",
    "--state-disabled-border",
    "--state-unavailable-text",
    "--state-unavailable-veil",
    "--state-destructive-accent",
    "--color-status-ready",
    "--color-status-attention",
    "--color-status-stopped",
    "--deco-plate-a-bg",
    "--deco-plate-a-fg",
    "--deco-plate-b-bg",
    "--deco-plate-b-fg",
    "--deco-plate-c-bg",
    "--deco-plate-c-fg",
    "--deco-plate-d-bg",
    "--deco-plate-d-fg",
    "--deco-plate-e-bg",
    "--deco-plate-e-fg",
    "--deco-plate-f-bg",
    "--deco-plate-f-fg",
    "--deco-aura-opacity",
    "--elev-1",
    "--elev-2",
    "--elev-focus",
];
const THEME_TOKENS: &[&str] = &[
    "--type-family-ui",
    "--type-family-display",
    "--type-family-plate",
    "--type-plate-size",
    "--type-hero-size",
    "--type-hero-weight",
    "--type-title-size",
    "--type-title-weight",
    "--type-h1-size",
    "--type-h1-weight",
    "--type-body-size",
    "--type-body-weight",
    "--type-label-size",
    "--type-label-weight",
    "--type-caption-size",
    "--type-caption-weight",
    "--type-eyebrow-size",
    "--type-eyebrow-weight",
    "--type-eyebrow-tracking",
    "--space-1",
    "--space-2",
    "--space-3",
    "--space-4",
    "--space-5",
    "--space-6",
    "--space-7",
    "--space-8",
    "--radius-s",
    "--radius-m",
    "--radius-l",
    "--radius-pill",
    "--motion-focus-duration",
    "--motion-shelf-duration",
    "--motion-route-duration",
    "--motion-launch-duration",
    "--motion-return-duration",
    "--motion-overlay-duration",
    "--motion-crash-duration",
    "--ease-out",
    "--ease-inout",
];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Base {
    Dusk,
    Day,
    HighContrast,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rgba {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: u8,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Length {
    pub pixels: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StyleValue {
    Color(Rgba),
    Length(Length),
}

/// Fully parsed, base-specific native-rendering values from `tokens.json`.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedStyleSnapshot {
    base: Base,
    values: BTreeMap<String, StyleValue>,
}

impl ResolvedStyleSnapshot {
    pub fn base(&self) -> Base {
        self.base
    }

    pub fn resolve(&self, key: &str) -> Result<StyleValue, ResolveError> {
        self.values
            .get(key)
            .copied()
            .ok_or_else(|| ResolveError::UnknownStyleKey(key.into()))
    }

    pub fn color(&self, key: &str) -> Result<Rgba, ResolveError> {
        match self.resolve(key)? {
            StyleValue::Color(value) => Ok(value),
            StyleValue::Length(_) => Err(ResolveError::StyleTypeMismatch(key.into())),
        }
    }

    pub fn length(&self, key: &str) -> Result<Length, ResolveError> {
        match self.resolve(key)? {
            StyleValue::Length(value) => Ok(value),
            StyleValue::Color(_) => Err(ResolveError::StyleTypeMismatch(key.into())),
        }
    }
}
impl Base {
    fn key(self) -> &'static str {
        match self {
            Self::Dusk => "dark",
            Self::Day => "light",
            Self::HighContrast => "high-contrast",
        }
    }
    fn css_selector(self) -> &'static str {
        match self {
            Self::Dusk => ":root",
            Self::Day => "[data-base=\"day\"]",
            Self::HighContrast => "[data-base=\"contrast\"]",
        }
    }
}
const BASES: [Base; 3] = [Base::Dusk, Base::Day, Base::HighContrast];

#[derive(Clone, Debug, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub theme_version: String,
    pub targets_vocabulary: u32,
    #[serde(default)]
    bases: BTreeMap<String, Coverage>,
    #[serde(default)]
    fonts: BTreeMap<String, String>,
    motion: MotionDeclaration,
    #[serde(default)]
    state_cues: BTreeMap<String, String>,
    #[serde(default)]
    decoration_slots: Vec<DecorationSlot>,
    #[serde(default)]
    assets: Vec<Asset>,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum Coverage {
    Full,
    Absent,
}
#[derive(Clone, Debug, Deserialize)]
struct MotionDeclaration {
    reduced_motion: String,
    table: BTreeMap<String, MotionValue>,
}
#[derive(Clone, Debug, Deserialize)]
struct MotionValue {
    duration_ms: u32,
    easing: String,
}
#[derive(Clone, Debug, Deserialize)]
struct DecorationSlot {
    #[allow(dead_code)]
    id: String,
    #[serde(default)]
    text_adjacent: bool,
    surface: Option<String>,
    #[serde(default)]
    contents: Vec<String>,
    #[serde(default)]
    motifs: Vec<String>,
}
#[derive(Clone, Debug, Deserialize)]
struct Asset {
    path: String,
    #[serde(rename = "type")]
    media_type: String,
    sha256: String,
}
#[derive(Clone, Debug, Deserialize)]
struct TokenDocument {
    #[serde(default)]
    theme: BTreeMap<String, String>,
    #[serde(default)]
    bases: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Clone, Debug)]
pub struct Theme {
    manifest: Manifest,
    tokens: TokenDocument,
}
impl Theme {
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    pub fn resolve(&self, base: Base, key: &str) -> Result<&str, ResolveError> {
        if !known_token(key) {
            return Err(ResolveError::UnknownToken(key.into()));
        }
        if THEME_TOKENS.contains(&key) {
            return self
                .tokens
                .theme
                .get(key)
                .map(String::as_str)
                .ok_or_else(|| ResolveError::AbsentToken {
                    base,
                    key: key.into(),
                });
        }
        if self.manifest.bases.get(base.key()) != Some(&Coverage::Full) {
            return Err(ResolveError::AbsentBase(base));
        }
        self.tokens
            .bases
            .get(base.key())
            .and_then(|v| v.get(key))
            .map(String::as_str)
            .ok_or_else(|| ResolveError::AbsentToken {
                base,
                key: key.into(),
            })
    }

    /// Resolves the semantic style key carried by a renderer-independent scene node.
    pub fn resolve_node<'a>(
        &'a self,
        base: Base,
        node: &pf_scene::Node,
    ) -> Result<&'a str, ResolveError> {
        self.resolve(base, &node.style_token)
    }

    pub fn resolved_style(&self, base: Base) -> Result<ResolvedStyleSnapshot, ResolveError> {
        let mut values = BTreeMap::new();
        for &key in STYLE_TOKENS {
            let raw = self.resolve(base, key)?;
            let value = if is_style_length(key) {
                StyleValue::Length(parse_length(raw).ok_or_else(|| {
                    ResolveError::InvalidResolvedValue {
                        key: key.into(),
                        value: raw.into(),
                    }
                })?)
            } else {
                StyleValue::Color(parse_rgba(raw).ok_or_else(|| {
                    ResolveError::InvalidResolvedValue {
                        key: key.into(),
                        value: raw.into(),
                    }
                })?)
            };
            values.insert(key.into(), value);
        }
        Ok(ResolvedStyleSnapshot { base, values })
    }
    pub fn resolve_motion(
        &self,
        intent: &str,
        reduced_motion: bool,
    ) -> Result<ResolvedMotion, ResolveError> {
        let value = self
            .manifest
            .motion
            .table
            .get(intent)
            .ok_or_else(|| ResolveError::UnknownMotion(intent.into()))?;
        Ok(ResolvedMotion {
            duration_ms: if reduced_motion { 0 } else { value.duration_ms },
            easing: value.easing.clone(),
        })
    }
    pub fn to_css(&self) -> String {
        let mut out = String::from("/* Generated by pf-theme from tokens.json; do not edit. */\n");
        write_block(&mut out, ":root", &self.tokens.theme);
        for base in BASES {
            if let Some(values) = self.tokens.bases.get(base.key()) {
                write_block(&mut out, base.css_selector(), values);
            }
        }
        out.push_str("[data-reduce-motion] {\n");
        for intent in MOTION {
            out.push_str(&format!("  --motion-{intent}-duration: 0ms;\n"));
        }
        out.push_str("}\n@media (prefers-reduced-motion: reduce) {\n  :root {\n");
        for intent in MOTION {
            out.push_str(&format!("    --motion-{intent}-duration: 0ms;\n"));
        }
        out.push_str("  }\n}\n");
        out
    }
}
fn write_block(out: &mut String, selector: &str, values: &BTreeMap<String, String>) {
    out.push_str(selector);
    out.push_str(" {\n");
    for (k, v) in values {
        out.push_str("  ");
        out.push_str(k);
        out.push_str(": ");
        out.push_str(v);
        out.push_str(";\n");
    }
    out.push_str("}\n");
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedMotion {
    pub duration_ms: u32,
    pub easing: String,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolveError {
    UnknownToken(String),
    AbsentBase(Base),
    AbsentToken { base: Base, key: String },
    UnknownMotion(String),
    UnknownStyleKey(String),
    StyleTypeMismatch(String),
    InvalidResolvedValue { key: String, value: String },
}

fn is_style_length(key: &str) -> bool {
    matches!(
        key,
        "--focus-ring-width" | "--focus-ring-offset" | "--state-pressed-shift"
    )
}

fn parse_length(value: &str) -> Option<Length> {
    Some(Length {
        pixels: value.strip_suffix("px")?.parse().ok()?,
    })
}

fn parse_rgba(value: &str) -> Option<Rgba> {
    let parsed = parse_color(value)?;
    Some(Rgba {
        red: (parsed[0] * 255.0).round() as u8,
        green: (parsed[1] * 255.0).round() as u8,
        blue: (parsed[2] * 255.0).round() as u8,
        alpha: (parsed[3] * 255.0).round() as u8,
    })
}
#[derive(Debug)]
pub enum LoadError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Json {
        file: &'static str,
        source: serde_json::Error,
    },
    UnsupportedVocabulary(u32),
    InvalidCoverage(String),
    MissingToken {
        base: Option<Base>,
        key: String,
    },
    UnknownToken(String),
    InvalidTokenValue {
        key: String,
        value: String,
    },
    InvalidFont(String),
    MissingStateCue(String),
    InvalidStateCue {
        state: String,
        expected: String,
        actual: String,
    },
    MotionMissing(String),
    MotionOutOfBounds {
        intent: String,
        duration_ms: u32,
    },
    InvalidEasing {
        intent: String,
        easing: String,
    },
    ReducedMotionNotStructural,
    InvalidColor {
        base: Base,
        key: String,
        value: String,
    },
    Contrast {
        base: Base,
        content: String,
        surface: String,
        ratio_milli: u32,
        floor_milli: u32,
    },
    ScrimStraddles {
        base: Base,
        content: String,
    },
    UnsafeAssetPath(String),
    Symlink(PathBuf),
    MissingAsset(String),
    AssetType {
        path: String,
        media_type: String,
    },
    AssetHash {
        path: String,
        expected: String,
        actual: String,
    },
    UnsafeAssetContent {
        path: String,
        reason: String,
    },
    UndeclaredMotif(String),
}
impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for LoadError {}
impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ResolveError {}

pub fn load(path: impl AsRef<Path>) -> Result<Theme, LoadError> {
    let root = path.as_ref();
    reject_symlinks(root)?;
    let manifest: Manifest = read_json(root.join(MANIFEST), MANIFEST)?;
    let tokens: TokenDocument = read_json(root.join(TOKENS), TOKENS)?;
    validate(root, &manifest, &tokens)?;
    Ok(Theme { manifest, tokens })
}
fn read_json<T: for<'a> Deserialize<'a>>(
    path: PathBuf,
    file: &'static str,
) -> Result<T, LoadError> {
    let data = fs::read_to_string(&path).map_err(|source| LoadError::Io { path, source })?;
    serde_json::from_str(&data).map_err(|source| LoadError::Json { file, source })
}

pub fn flagship() -> Theme {
    let manifest = serde_json::from_str(include_str!("../vendor/package/manifest.json"))
        .expect("embedded flagship manifest");
    // Fixture source: pocketforge-os/design/theme-quiet-console/package/tokens.json
    // at design commit d5b97d97430ec67ccedbe1508e4c55a184843f8c.
    let tokens = serde_json::from_str(include_str!("../vendor/package/tokens.json"))
        .expect("embedded flagship tokens");
    Theme { manifest, tokens }
}
pub fn load_or_flagship(path: impl AsRef<Path>) -> (Theme, Option<LoadError>) {
    match load(path) {
        Ok(t) => (t, None),
        Err(e) => (flagship(), Some(e)),
    }
}

const MOTION: [&str; 7] = [
    "focus", "shelf", "route", "launch", "return", "overlay", "crash",
];
const MOTION_MAX: [u32; 7] = [200, 350, 350, 700, 450, 280, 500];
const STATES: [(&str, &str); 7] = [
    ("rest", "none"),
    ("focused", "ring-shape+scale"),
    ("selected", "pip-or-inset-bar"),
    ("pressed", "positional-inset-offset"),
    ("disabled", "dashed-border+em-dash"),
    ("unavailable", "slash-glyph+reason-text"),
    ("destructive", "warn-glyph+confirm-language"),
];
fn known_token(k: &str) -> bool {
    BASE_TOKENS.contains(&k) || THEME_TOKENS.contains(&k)
}
fn validate_token_value(key: &str, value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if value.is_empty()
        || value.chars().any(char::is_control)
        || [';', '{', '}', '@'].iter().any(|c| value.contains(*c))
        || lower.contains("url(")
    {
        return false;
    }

    if key.starts_with("--type-family-") {
        return value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_'));
    }
    if key.ends_with("-weight") {
        return value.parse::<u16>().is_ok_and(|n| (1..=1000).contains(&n))
            && value.chars().all(|c| c.is_ascii_digit());
    }
    if key.ends_with("-duration") {
        return parse_nonnegative_number(value.strip_suffix("ms").unwrap_or(""));
    }
    if key.starts_with("--ease-") {
        return parse_cubic_bezier(value);
    }
    if key == "--deco-aura-opacity" {
        return parse_unit_interval(value);
    }
    if key.starts_with("--elev-") {
        return value == "none" || parse_shadow(value);
    }
    if key.ends_with("-size")
        || key.ends_with("-tracking")
        || key.starts_with("--space-")
        || key.starts_with("--radius-")
        || key == "--focus-ring-width"
        || key == "--focus-ring-offset"
        || key == "--state-pressed-shift"
    {
        return ["px", "rem", "em"].iter().any(|unit| {
            value
                .strip_suffix(unit)
                .is_some_and(parse_nonnegative_number)
        });
    }
    parse_color(value).is_some()
}
fn parse_nonnegative_number(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().filter(|b| *b == b'.').count() <= 1
        && s.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && s.parse::<f64>().is_ok_and(|n| n.is_finite() && n >= 0.0)
}
fn parse_unit_interval(s: &str) -> bool {
    parse_nonnegative_number(s) && s.parse::<f64>().is_ok_and(|n| n <= 1.0)
}
fn parse_cubic_bezier(s: &str) -> bool {
    let Some(body) = s
        .strip_prefix("cubic-bezier(")
        .and_then(|s| s.strip_suffix(')'))
    else {
        return false;
    };
    let parts: Vec<_> = body.split(',').map(str::trim).collect();
    parts.len() == 4 && parts.into_iter().all(parse_unit_interval)
}
fn parse_shadow(s: &str) -> bool {
    let Some(color_at) = s.find("rgba(") else {
        return false;
    };
    let dimensions: Vec<_> = s[..color_at].split_whitespace().collect();
    dimensions.len() == 3
        && dimensions
            .iter()
            .all(|v| *v == "0" || v.strip_suffix("px").is_some_and(parse_nonnegative_number))
        && parse_color(s[color_at..].trim()).is_some()
}
fn validate(root: &Path, m: &Manifest, t: &TokenDocument) -> Result<(), LoadError> {
    if m.targets_vocabulary > 1 {
        return Err(LoadError::UnsupportedVocabulary(m.targets_vocabulary));
    }
    for k in t
        .theme
        .keys()
        .chain(t.bases.values().flat_map(|b| b.keys()))
    {
        if !known_token(k) {
            return Err(LoadError::UnknownToken(k.clone()));
        }
    }
    for (key, value) in t
        .theme
        .iter()
        .chain(t.bases.values().flat_map(|base| base.iter()))
    {
        if !validate_token_value(key, value) {
            return Err(LoadError::InvalidTokenValue {
                key: key.clone(),
                value: value.clone(),
            });
        }
    }
    for k in THEME_TOKENS {
        if !t.theme.contains_key(*k) {
            return Err(LoadError::MissingToken {
                base: None,
                key: (*k).into(),
            });
        }
    }
    for base in BASES {
        match m.bases.get(base.key()).copied().unwrap_or(Coverage::Absent) {
            Coverage::Full => {
                let values = t
                    .bases
                    .get(base.key())
                    .ok_or_else(|| LoadError::MissingToken {
                        base: Some(base),
                        key: BASE_TOKENS[0].into(),
                    })?;
                for k in BASE_TOKENS {
                    if !values.contains_key(*k) {
                        return Err(LoadError::MissingToken {
                            base: Some(base),
                            key: (*k).into(),
                        });
                    }
                }
            }
            Coverage::Absent => {
                if t.bases.get(base.key()).is_some_and(|v| !v.is_empty()) {
                    return Err(LoadError::InvalidCoverage(base.key().into()));
                }
            }
        }
    }
    for font in m.fonts.values() {
        if font != "Manrope" && font != "Fraunces" {
            return Err(LoadError::InvalidFont(font.clone()));
        }
    }
    for (state, expected) in STATES {
        let actual = m
            .state_cues
            .get(state)
            .ok_or_else(|| LoadError::MissingStateCue(state.into()))?;
        if actual != expected {
            return Err(LoadError::InvalidStateCue {
                state: state.into(),
                expected: expected.into(),
                actual: actual.clone(),
            });
        }
    }
    if m.motion.reduced_motion != "complete-stop" {
        return Err(LoadError::ReducedMotionNotStructural);
    }
    for (i, intent) in MOTION.iter().enumerate() {
        let v = m
            .motion
            .table
            .get(*intent)
            .ok_or_else(|| LoadError::MotionMissing((*intent).into()))?;
        if v.duration_ms > MOTION_MAX[i] {
            return Err(LoadError::MotionOutOfBounds {
                intent: (*intent).into(),
                duration_ms: v.duration_ms,
            });
        }
        if !["decel", "in-out", "linear"].contains(&v.easing.as_str()) {
            return Err(LoadError::InvalidEasing {
                intent: (*intent).into(),
                easing: v.easing.clone(),
            });
        }
    }
    validate_contrast(m, t)?;
    validate_assets(root, m)?;
    Ok(())
}

const CONTRAST: [(&str, &str, f64, f64); 19] = [
    ("--color-text-primary", "--color-surface-canvas", 4.5, 7.0),
    ("--color-text-primary", "--color-surface-raised", 4.5, 7.0),
    ("--color-text-primary", "--color-surface-overlay", 4.5, 7.0),
    ("--color-text-secondary", "--color-surface-canvas", 4.5, 7.0),
    ("--color-text-secondary", "--color-surface-raised", 4.5, 7.0),
    ("--color-text-muted", "--color-surface-canvas", 4.5, 7.0),
    ("--color-text-muted", "--color-surface-raised", 4.5, 7.0),
    ("--color-text-inverse", "--color-focus-ring", 4.5, 7.0),
    ("--color-focus-ring", "--color-surface-canvas", 3.0, 3.0),
    ("--color-focus-ring", "--color-surface-raised", 3.0, 3.0),
    (
        "--state-selected-accent",
        "--color-surface-raised",
        3.0,
        3.0,
    ),
    (
        "--state-unavailable-text",
        "--color-surface-raised",
        4.5,
        7.0,
    ),
    (
        "--state-destructive-accent",
        "--color-surface-raised",
        4.5,
        7.0,
    ),
    ("--color-status-ready", "--color-surface-canvas", 4.5, 7.0),
    ("--color-status-ready", "--color-surface-raised", 4.5, 7.0),
    (
        "--color-status-attention",
        "--color-surface-canvas",
        4.5,
        7.0,
    ),
    (
        "--color-status-attention",
        "--color-surface-raised",
        4.5,
        7.0,
    ),
    ("--color-status-stopped", "--color-surface-canvas", 4.5, 7.0),
    ("--color-status-stopped", "--color-surface-raised", 4.5, 7.0),
];
fn validate_contrast(m: &Manifest, t: &TokenDocument) -> Result<(), LoadError> {
    for base in BASES {
        if m.bases.get(base.key()) != Some(&Coverage::Full) {
            continue;
        }
        let v = &t.bases[base.key()];
        for (c, s, f, h) in CONTRAST {
            check_pair(
                base,
                v,
                c,
                s,
                if base == Base::HighContrast { h } else { f },
            )?;
        }
        for slot in &m.decoration_slots {
            if !slot.text_adjacent {
                continue;
            }
            let surface = slot
                .surface
                .as_ref()
                .ok_or_else(|| LoadError::UnsafeAssetContent {
                    path: slot.id.clone(),
                    reason: "text-adjacent slot has no surface".into(),
                })?;
            let scrim = parse_color(v.get(surface).ok_or_else(|| LoadError::MissingToken {
                base: Some(base),
                key: surface.clone(),
            })?)
            .ok_or_else(|| LoadError::InvalidColor {
                base,
                key: surface.clone(),
                value: v[surface].clone(),
            })?;
            for c in &slot.contents {
                let floor = if c == "--state-selected-accent" {
                    3.0
                } else if base == Base::HighContrast {
                    7.0
                } else {
                    4.5
                };
                let fg = parse_color(v.get(c).ok_or_else(|| LoadError::MissingToken {
                    base: Some(base),
                    key: c.clone(),
                })?)
                .ok_or_else(|| LoadError::InvalidColor {
                    base,
                    key: c.clone(),
                    value: v[c].clone(),
                })?;
                let lo = [
                    scrim[0] * scrim[3],
                    scrim[1] * scrim[3],
                    scrim[2] * scrim[3],
                    1.0,
                ];
                let hi = [
                    lo[0] + 1.0 - scrim[3],
                    lo[1] + 1.0 - scrim[3],
                    lo[2] + 1.0 - scrim[3],
                    1.0,
                ];
                let lt = lum(fg);
                let (l0, l1) = (lum(lo), lum(hi));
                if contrast(lt, l0) < floor || contrast(lt, l1) < floor {
                    return Err(LoadError::Contrast {
                        base,
                        content: c.clone(),
                        surface: surface.clone(),
                        ratio_milli: (contrast(lt, l0).min(contrast(lt, l1)) * 1000.0) as u32,
                        floor_milli: (floor * 1000.0) as u32,
                    });
                }
                if lt > l0 && lt < l1 {
                    return Err(LoadError::ScrimStraddles {
                        base,
                        content: c.clone(),
                    });
                }
            }
        }
    }
    Ok(())
}
fn check_pair(
    base: Base,
    v: &BTreeMap<String, String>,
    c: &str,
    s: &str,
    floor: f64,
) -> Result<(), LoadError> {
    let pc = parse_color(&v[c]).ok_or_else(|| LoadError::InvalidColor {
        base,
        key: c.into(),
        value: v[c].clone(),
    })?;
    let ps = parse_color(&v[s]).ok_or_else(|| LoadError::InvalidColor {
        base,
        key: s.into(),
        value: v[s].clone(),
    })?;
    let ratio = contrast(lum(pc), lum(ps));
    if ratio + 0.0001 < floor {
        return Err(LoadError::Contrast {
            base,
            content: c.into(),
            surface: s.into(),
            ratio_milli: (ratio * 1000.0) as u32,
            floor_milli: (floor * 1000.0) as u32,
        });
    }
    Ok(())
}
fn parse_color(s: &str) -> Option<[f64; 4]> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix('#') {
        if h.len() != 6 && h.len() != 8 {
            return None;
        }
        let x = u32::from_str_radix(h, 16).ok()?;
        return Some(if h.len() == 6 {
            [
                ((x >> 16) & 255) as f64 / 255.0,
                ((x >> 8) & 255) as f64 / 255.0,
                (x & 255) as f64 / 255.0,
                1.0,
            ]
        } else {
            [
                ((x >> 24) & 255) as f64 / 255.0,
                ((x >> 16) & 255) as f64 / 255.0,
                ((x >> 8) & 255) as f64 / 255.0,
                (x & 255) as f64 / 255.0,
            ]
        });
    }
    let body = s.strip_prefix("rgba(")?.strip_suffix(')')?;
    let p: Vec<_> = body.split(',').map(str::trim).collect();
    if p.len() != 4 {
        return None;
    }
    let rgb = [
        p[0].parse::<u8>().ok()?,
        p[1].parse::<u8>().ok()?,
        p[2].parse::<u8>().ok()?,
    ];
    let alpha = p[3].parse::<f64>().ok()?;
    if !alpha.is_finite() || !(0.0..=1.0).contains(&alpha) {
        return None;
    }
    Some([
        rgb[0] as f64 / 255.0,
        rgb[1] as f64 / 255.0,
        rgb[2] as f64 / 255.0,
        alpha,
    ])
}
fn lum(c: [f64; 4]) -> f64 {
    fn lin(x: f64) -> f64 {
        if x <= 0.04045 {
            x / 12.92
        } else {
            ((x + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
}
fn contrast(a: f64, b: f64) -> f64 {
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[cfg(test)]
mod resolved_style_tests {
    use super::*;

    #[test]
    fn every_native_style_token_is_exactly_typed_in_all_bases() {
        let theme = flagship();
        for base in BASES {
            let snapshot = theme.resolved_style(base).unwrap();
            assert_eq!(snapshot.base(), base);
            for &key in STYLE_TOKENS {
                let raw = theme.resolve(base, key).unwrap();
                let expected = if is_style_length(key) {
                    StyleValue::Length(parse_length(raw).unwrap())
                } else {
                    StyleValue::Color(parse_rgba(raw).unwrap())
                };
                assert_eq!(snapshot.resolve(key).unwrap(), expected, "{base:?} {key}");
            }
        }
    }

    #[test]
    fn representative_ruled_values_match_tokens_json() {
        let theme = flagship();
        let samples = [
            ("--color-surface-canvas", ["#171512", "#f2eee4", "#000000"]),
            ("--color-focus-ring", ["#f3dfae", "#8a6414", "#ffd83d"]),
            (
                "--state-destructive-accent",
                ["#e07a6e", "#a83a2e", "#ff8d80"],
            ),
        ];
        for (key, expected) in samples {
            for (index, base) in BASES.into_iter().enumerate() {
                assert_eq!(theme.resolve(base, key).unwrap(), expected[index]);
                assert_eq!(
                    theme.resolved_style(base).unwrap().color(key).unwrap(),
                    parse_rgba(expected[index]).unwrap()
                );
            }
        }
        assert_eq!(
            theme
                .resolved_style(Base::Dusk)
                .unwrap()
                .color("--deco-plate-a-bg")
                .unwrap()
                .alpha,
            0x48
        );
    }
}

fn reject_symlinks(root: &Path) -> Result<(), LoadError> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).map_err(|source| LoadError::Io {
            path: dir.clone(),
            source,
        })? {
            let e = e.map_err(|source| LoadError::Io {
                path: dir.clone(),
                source,
            })?;
            let ty = e.file_type().map_err(|source| LoadError::Io {
                path: e.path(),
                source,
            })?;
            if ty.is_symlink() {
                return Err(LoadError::Symlink(e.path()));
            }
            if ty.is_dir() {
                stack.push(e.path())
            }
        }
    }
    Ok(())
}
fn safe_rel(s: &str) -> bool {
    let p = Path::new(s);
    !p.is_absolute() && p.components().all(|c| matches!(c, Component::Normal(_)))
}
fn validate_assets(root: &Path, m: &Manifest) -> Result<(), LoadError> {
    // Containment is the first asset gate so a hostile declaration is never
    // obscured by a secondary relationship error.
    for asset in &m.assets {
        if !safe_rel(&asset.path) {
            return Err(LoadError::UnsafeAssetPath(asset.path.clone()));
        }
    }
    let declared: BTreeSet<_> = m.assets.iter().map(|a| a.path.as_str()).collect();
    for slot in &m.decoration_slots {
        for motif in &slot.motifs {
            if !declared.contains(motif.as_str()) {
                return Err(LoadError::UndeclaredMotif(motif.clone()));
            }
        }
    }
    for a in &m.assets {
        if !matches!(
            (
                Path::new(&a.path).extension().and_then(|x| x.to_str()),
                a.media_type.as_str()
            ),
            (Some("svg"), "image/svg+xml") | (Some("css"), "text/css")
        ) {
            return Err(LoadError::AssetType {
                path: a.path.clone(),
                media_type: a.media_type.clone(),
            });
        }
        let p = root.join(&a.path);
        let bytes = fs::read(&p).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                LoadError::MissingAsset(a.path.clone())
            } else {
                LoadError::Io {
                    path: p.clone(),
                    source: e,
                }
            }
        })?;
        let actual = sha256_hex(&bytes);
        if !actual.eq_ignore_ascii_case(&a.sha256) {
            return Err(LoadError::AssetHash {
                path: a.path.clone(),
                expected: a.sha256.clone(),
                actual,
            });
        }
        let text = String::from_utf8_lossy(&bytes);
        let lower = text.to_ascii_lowercase();
        if a.media_type == "text/css"
            && [
                "@",
                "url(",
                "expression(",
                "javascript:",
                "<script",
                "behavior:",
            ]
            .iter()
            .any(|x| lower.contains(x))
        {
            return Err(LoadError::UnsafeAssetContent {
                path: a.path.clone(),
                reason: "CSS external or executable construct".into(),
            });
        }
        if a.media_type == "image/svg+xml" {
            validate_svg(&a.path, &text)?
        }
    }
    Ok(())
}
fn validate_svg(path: &str, s: &str) -> Result<(), LoadError> {
    let lower = s.to_ascii_lowercase();
    if ["<!doctype", "<!entity", "<?", "<script", "javascript:"]
        .iter()
        .any(|x| lower.contains(x))
    {
        return Err(LoadError::UnsafeAssetContent {
            path: path.into(),
            reason: "active SVG construct".into(),
        });
    }
    let document =
        roxmltree::Document::parse(s).map_err(|error| LoadError::UnsafeAssetContent {
            path: path.into(),
            reason: format!("invalid SVG XML: {error}"),
        })?;
    for attribute in document
        .descendants()
        .filter(|node| node.is_element())
        .flat_map(|node| node.attributes())
    {
        let name = attribute.name();
        if name.eq_ignore_ascii_case("href") {
            return Err(LoadError::UnsafeAssetContent {
                path: path.into(),
                reason: format!("external-reference attribute {name}"),
            });
        }
        if name.len() > 2 && name[..2].eq_ignore_ascii_case("on") {
            return Err(LoadError::UnsafeAssetContent {
                path: path.into(),
                reason: format!("event attribute {name}"),
            });
        }
    }
    Ok(())
}

// Small in-crate SHA-256 avoids adding a crypto dependency solely for package integrity.
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut msg = data.to_vec();
    let bits = (msg.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0)
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    let mut h = [
        0x6a09e667u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, x) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes(x.try_into().unwrap())
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1)
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut z) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = z
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            z = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2)
        }
        for (i, v) in [a, b, c, d, e, f, g, z].iter().enumerate() {
            h[i] = h[i].wrapping_add(*v)
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

#[cfg(test)]
mod unit {
    use super::*;
    #[test]
    fn sha() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        )
    }
}
