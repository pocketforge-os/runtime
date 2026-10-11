//! Strict validator and deterministic CBOR codec for PocketForge problem report v1.
//!
//! The wire contract and cross-implementation fixtures live in
//! `platform/schemas/problem-report-v1`. This crate intentionally implements
//! only that closed grammar: accepting general CBOR would enlarge the input
//! surface without adding a report feature.

use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};
use std::error::Error;
use std::fmt;

pub const MAX_BYTES: usize = 4096;
const MAX_FRAMES: usize = 48;
const MAX_SAMPLES: usize = 2;
const MAX_KFRAMES: usize = 32;
const MAX_OFFSET: u64 = (1_u64 << 48) - 1;
const MAX_KOFFSET: u64 = u32::MAX as u64;
const MAX_DEPTH: usize = 8;

const KINDS: &[&str] = &["crash", "hang", "oom", "component", "kernel"];
const SIGNALS: &[&str] = &[
    "SIGSEGV", "SIGABRT", "SIGBUS", "SIGILL", "SIGFPE", "SIGTRAP", "SIGSYS", "SIGQUIT", "SIGXCPU",
    "SIGXFSZ",
];
const ORIGINS: &[&str] = &["image", "catalog", "sideloaded"];
const BUCKETS: &[&str] = &["<1m", "1-10m", "10-60m", ">1h"];
const TOP_LEVEL_FIELDS: &[&str] = &[
    "v", "kind", "how", "day", "build", "model", "app", "uptime", "session", "mem", "frames",
    "samples", "kframes", "panic_at", "del",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefusalReason {
    TooLarge,
    Malformed,
    Unsupported,
    NonCanonical,
    Type,
    UnknownField,
    Version,
    MissingField,
    ForbiddenField,
    Vocabulary,
    Grammar,
    Range,
    Inconsistent,
}

impl RefusalReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TooLarge => "too-large",
            Self::Malformed => "malformed",
            Self::Unsupported => "unsupported",
            Self::NonCanonical => "non-canonical",
            Self::Type => "type",
            Self::UnknownField => "unknown-field",
            Self::Version => "version",
            Self::MissingField => "missing-field",
            Self::ForbiddenField => "forbidden-field",
            Self::Vocabulary => "vocabulary",
            Self::Grammar => "grammar",
            Self::Range => "range",
            Self::Inconsistent => "inconsistent",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Refused {
    reason: RefusalReason,
    at: String,
}

impl Refused {
    fn new(reason: RefusalReason, at: impl Into<String>) -> Self {
        Self {
            reason,
            at: at.into(),
        }
    }

    pub const fn reason(&self) -> RefusalReason {
        self.reason
    }

    pub fn at(&self) -> &str {
        &self.at
    }
}

impl fmt::Display for Refused {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} at {}",
            self.reason.as_str(),
            if self.at.is_empty() {
                "<report>"
            } else {
                &self.at
            }
        )
    }
}

impl Error for Refused {}

type Result<T> = std::result::Result<T, Refused>;

#[derive(Clone, Debug, Eq, PartialEq)]
enum CborValue {
    Null,
    Bool(bool),
    Uint(u64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<CborValue>),
    Map(Vec<(String, CborValue)>),
}

impl CborValue {
    fn field(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Map(entries) => entries
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    fn fields(&self) -> Result<&[(String, Self)]> {
        match self {
            Self::Map(entries) => Ok(entries),
            _ => Err(refused(RefusalReason::Type, "")),
        }
    }

    fn text(&self, at: &str) -> Result<&str> {
        match self {
            Self::Text(value) => Ok(value),
            _ => Err(refused(RefusalReason::Type, at)),
        }
    }

    fn array(&self, at: &str) -> Result<&[Self]> {
        match self {
            Self::Array(value) => Ok(value),
            _ => Err(refused(RefusalReason::Type, at)),
        }
    }
}

fn refused(reason: RefusalReason, at: impl Into<String>) -> Refused {
    Refused::new(reason, at)
}

fn push_byte(byte: u8, output: &mut Vec<u8>) -> Result<()> {
    if output.len() >= MAX_BYTES {
        return Err(refused(RefusalReason::TooLarge, ""));
    }
    output.push(byte);
    Ok(())
}

fn push_bytes(bytes: &[u8], output: &mut Vec<u8>) -> Result<()> {
    if output
        .len()
        .checked_add(bytes.len())
        .is_none_or(|length| length > MAX_BYTES)
    {
        return Err(refused(RefusalReason::TooLarge, ""));
    }
    output.extend_from_slice(bytes);
    Ok(())
}

fn head(major: u8, argument: u64, output: &mut Vec<u8>) -> Result<()> {
    match argument {
        0..=23 => push_byte((major << 5) | argument as u8, output)?,
        24..=0xff => {
            push_byte((major << 5) | 24, output)?;
            push_byte(argument as u8, output)?;
        }
        0x100..=0xffff => {
            push_byte((major << 5) | 25, output)?;
            push_bytes(&(argument as u16).to_be_bytes(), output)?;
        }
        0x1_0000..=0xffff_ffff => {
            push_byte((major << 5) | 26, output)?;
            push_bytes(&(argument as u32).to_be_bytes(), output)?;
        }
        _ => {
            push_byte((major << 5) | 27, output)?;
            push_bytes(&argument.to_be_bytes(), output)?;
        }
    }
    Ok(())
}

fn encode_value(value: &CborValue, output: &mut Vec<u8>) -> Result<()> {
    match value {
        CborValue::Null => push_byte(0xf6, output)?,
        CborValue::Bool(false) => push_byte(0xf4, output)?,
        CborValue::Bool(true) => push_byte(0xf5, output)?,
        CborValue::Uint(value) => head(0, *value, output)?,
        CborValue::Bytes(value) => {
            head(2, value.len() as u64, output)?;
            push_bytes(value, output)?;
        }
        CborValue::Text(value) => {
            head(3, value.len() as u64, output)?;
            push_bytes(value.as_bytes(), output)?;
        }
        CborValue::Array(values) => {
            head(4, values.len() as u64, output)?;
            for value in values {
                encode_value(value, output)?;
            }
        }
        CborValue::Map(entries) => {
            let mut pairs = entries
                .iter()
                .map(|(key, value)| -> Result<_> {
                    let mut encoded_key = Vec::new();
                    head(3, key.len() as u64, &mut encoded_key)?;
                    push_bytes(key.as_bytes(), &mut encoded_key)?;
                    Ok((encoded_key, value))
                })
                .collect::<Result<Vec<_>>>()?;
            pairs.sort_by(|left, right| left.0.cmp(&right.0));
            head(5, pairs.len() as u64, output)?;
            for (key, value) in pairs {
                push_bytes(&key, output)?;
                encode_value(value, output)?;
            }
        }
    }
    Ok(())
}

struct Parser<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Parser<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn parse(mut self) -> Result<CborValue> {
        let value = self.item(0)?;
        if self.position != self.data.len() {
            return Err(refused(RefusalReason::Malformed, "trailing bytes"));
        }
        Ok(value)
    }

    fn take(&mut self) -> Result<u8> {
        let byte = self
            .data
            .get(self.position)
            .copied()
            .ok_or_else(|| refused(RefusalReason::Malformed, "truncated"))?;
        self.position += 1;
        Ok(byte)
    }

    fn argument(&mut self, additional: u8) -> Result<u64> {
        if additional < 24 {
            return Ok(u64::from(additional));
        }
        let width = match additional {
            24 => 1,
            25 => 2,
            26 => 4,
            27 => 8,
            28..=30 => return Err(refused(RefusalReason::Malformed, "reserved")),
            31 => return Err(refused(RefusalReason::Unsupported, "indefinite")),
            _ => unreachable!(),
        };
        let end = self
            .position
            .checked_add(width)
            .filter(|end| *end <= self.data.len())
            .ok_or_else(|| refused(RefusalReason::Malformed, "truncated"))?;
        let mut argument = 0_u64;
        for byte in &self.data[self.position..end] {
            argument = (argument << 8) | u64::from(*byte);
        }
        self.position = end;
        let minimum = match width {
            1 => 24,
            2 => 1 << 8,
            4 => 1 << 16,
            8 => 1 << 32,
            _ => unreachable!(),
        };
        if argument < minimum {
            return Err(refused(
                RefusalReason::NonCanonical,
                "argument not shortest",
            ));
        }
        Ok(argument)
    }

    fn item(&mut self, depth: usize) -> Result<CborValue> {
        let initial = self.take()?;
        let major = initial >> 5;
        let additional = initial & 31;
        if major == 7 {
            return match additional {
                20 => Ok(CborValue::Bool(false)),
                21 => Ok(CborValue::Bool(true)),
                22 => Ok(CborValue::Null),
                28..=30 => Err(refused(RefusalReason::Malformed, "reserved")),
                _ => Err(refused(RefusalReason::Unsupported, "simple/float")),
            };
        }
        if matches!(major, 1 | 6) {
            return Err(refused(RefusalReason::Unsupported, "negative/tag"));
        }
        if matches!(additional, 28..=30) {
            return Err(refused(RefusalReason::Malformed, "reserved"));
        }
        if additional == 31 {
            return Err(refused(
                if major == 0 {
                    RefusalReason::Malformed
                } else {
                    RefusalReason::Unsupported
                },
                "indefinite",
            ));
        }
        let argument = self.argument(additional)?;
        match major {
            0 => Ok(CborValue::Uint(argument)),
            2 | 3 => {
                let length = usize::try_from(argument)
                    .map_err(|_| refused(RefusalReason::Malformed, "length"))?;
                let end = self
                    .position
                    .checked_add(length)
                    .filter(|end| *end <= self.data.len())
                    .ok_or_else(|| refused(RefusalReason::Malformed, "truncated"))?;
                let bytes = &self.data[self.position..end];
                self.position = end;
                if major == 2 {
                    Ok(CborValue::Bytes(bytes.to_vec()))
                } else {
                    let text = std::str::from_utf8(bytes)
                        .map_err(|_| refused(RefusalReason::Malformed, "utf-8"))?;
                    Ok(CborValue::Text(text.to_owned()))
                }
            }
            4 | 5 if depth >= MAX_DEPTH => Err(refused(RefusalReason::Unsupported, "nesting")),
            4 => {
                let count = usize::try_from(argument)
                    .map_err(|_| refused(RefusalReason::Malformed, "length"))?;
                if count > self.data.len().saturating_sub(self.position) {
                    return Err(refused(RefusalReason::Malformed, "truncated"));
                }
                let mut values = Vec::with_capacity(count);
                for _ in 0..count {
                    values.push(self.item(depth + 1)?);
                }
                Ok(CborValue::Array(values))
            }
            5 => {
                let count = usize::try_from(argument)
                    .map_err(|_| refused(RefusalReason::Malformed, "length"))?;
                if count > self.data.len().saturating_sub(self.position) / 2 {
                    return Err(refused(RefusalReason::Malformed, "truncated"));
                }
                let mut entries = Vec::with_capacity(count);
                let mut previous = Vec::new();
                for _ in 0..count {
                    let start = self.position;
                    let key = self.item(depth + 1)?;
                    let CborValue::Text(key) = key else {
                        return Err(refused(RefusalReason::Unsupported, "non-text key"));
                    };
                    let raw_key = self.data[start..self.position].to_vec();
                    if !previous.is_empty() && raw_key <= previous {
                        return Err(refused(
                            RefusalReason::NonCanonical,
                            if raw_key == previous {
                                "duplicate key"
                            } else {
                                "key order"
                            },
                        ));
                    }
                    previous = raw_key;
                    entries.push((key, self.item(depth + 1)?));
                }
                Ok(CborValue::Map(entries))
            }
            _ => Err(refused(RefusalReason::Unsupported, "major type")),
        }
    }
}

fn map<'a>(value: &'a CborValue, at: &str) -> Result<&'a [(String, CborValue)]> {
    match value {
        CborValue::Map(entries) => Ok(entries),
        _ => Err(refused(RefusalReason::Type, at)),
    }
}

fn array<'a>(value: &'a CborValue, at: &str) -> Result<&'a [CborValue]> {
    value.array(at)
}

fn uint(value: &CborValue, at: &str) -> Result<u64> {
    match value {
        CborValue::Uint(value) => Ok(*value),
        _ => Err(refused(RefusalReason::Type, at)),
    }
}

fn boolean(value: &CborValue, at: &str) -> Result<bool> {
    match value {
        CborValue::Bool(value) => Ok(*value),
        _ => Err(refused(RefusalReason::Type, at)),
    }
}

fn closed_map<'a>(
    value: &'a CborValue,
    at: &str,
    required: &[&str],
    optional: &[&str],
) -> Result<&'a [(String, CborValue)]> {
    let entries = map(value, at)?;
    for (key, _) in entries {
        if !required.contains(&key.as_str()) && !optional.contains(&key.as_str()) {
            return Err(refused(RefusalReason::UnknownField, format!("{at}.{key}")));
        }
    }
    for key in required {
        if !entries.iter().any(|(name, _)| name == key) {
            return Err(refused(RefusalReason::MissingField, format!("{at}.{key}")));
        }
    }
    Ok(entries)
}

fn field<'a>(entries: &'a [(String, CborValue)], key: &str) -> Option<&'a CborValue> {
    entries
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

fn ascii_grammar(
    value: &CborValue,
    at: &str,
    limit: usize,
    predicate: impl Fn(&str) -> bool,
) -> Result<()> {
    let value = value.text(at)?;
    if value.is_ascii() && value.len() <= limit && predicate(value) {
        Ok(())
    } else {
        Err(refused(RefusalReason::Grammar, at))
    }
}

fn valid_day(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    if !bytes
        .iter()
        .enumerate()
        .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
    {
        return false;
    }
    let year: u32 = value[0..4].parse().unwrap_or(0);
    let month: usize = value[5..7].parse().unwrap_or(0);
    let day: u32 = value[8..10].parse().unwrap_or(0);
    if !(2020..=2099).contains(&year) || !(1..=12).contains(&month) {
        return false;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    (1..=days[month - 1]).contains(&day)
}

fn valid_model(value: &str) -> bool {
    (1..=32).contains(&value.len())
        && ascii_lowercase_or_digit(value.as_bytes()[0])
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn ascii_lowercase_or_digit(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit()
}

fn valid_app_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    !bytes.is_empty()
        && ascii_lowercase_or_digit(bytes[0])
        && ascii_lowercase_or_digit(bytes[bytes.len() - 1])
        && bytes
            .iter()
            .all(|byte| ascii_lowercase_or_digit(*byte) || matches!(byte, b'_' | b'-'))
}

fn valid_app_id(value: &str) -> bool {
    value.is_ascii()
        && (3..=200).contains(&value.len())
        && value.split('.').count() >= 2
        && value.split('.').all(valid_app_label)
}

fn valid_version(value: &str) -> bool {
    if value.is_empty() || value.len() > 32 {
        return false;
    }
    let parts: Vec<_> = value.split('.').collect();
    (1..=4).contains(&parts.len())
        && parts.iter().all(|part| {
            (1..=9).contains(&part.len())
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (part == &"0" || !part.starts_with('0'))
        })
}

fn valid_panic_at(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("crates/") else {
        return false;
    };
    let Some((path, column)) = rest.rsplit_once(':') else {
        return false;
    };
    let Some((path, line)) = path.rsplit_once(':') else {
        return false;
    };
    path.ends_with(".rs")
        && !path.is_empty()
        && path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'/' | b'.' | b'-'))
        && !line.is_empty()
        && line.bytes().all(|byte| byte.is_ascii_digit())
        && !column.is_empty()
        && column.bytes().all(|byte| byte.is_ascii_digit())
}

fn validate_app(value: &CborValue) -> Result<()> {
    let app = closed_map(value, "app", &["origin"], &["id", "ver"])?;
    let origin = field(app, "origin").expect("required").text("app.origin")?;
    if !ORIGINS.contains(&origin) {
        return Err(refused(RefusalReason::Vocabulary, "app.origin"));
    }
    if origin == "sideloaded" {
        for key in ["id", "ver"] {
            if field(app, key).is_some() {
                return Err(refused(RefusalReason::ForbiddenField, format!("app.{key}")));
            }
        }
    }
    if origin != "sideloaded" && field(app, "id").is_none() {
        return Err(refused(RefusalReason::MissingField, "app.id"));
    }
    if origin == "catalog" && field(app, "ver").is_none() {
        return Err(refused(RefusalReason::MissingField, "app.ver"));
    }
    if let Some(id) = field(app, "id") {
        ascii_grammar(id, "app.id", 200, valid_app_id)?;
    }
    if let Some(version) = field(app, "ver") {
        ascii_grammar(version, "app.ver", 32, valid_version)?;
    }
    Ok(())
}

fn validate_mem(value: &CborValue) -> Result<()> {
    let mem = closed_map(value, "mem", &["oom", "pressure"], &[])?;
    boolean(field(mem, "oom").expect("required"), "mem.oom")?;
    boolean(field(mem, "pressure").expect("required"), "mem.pressure")?;
    Ok(())
}

fn validate_frames(value: &CborValue) -> Result<()> {
    let frames = array(value, "frames")?;
    if frames.len() > MAX_FRAMES {
        return Err(refused(RefusalReason::Range, "frames"));
    }
    for (index, frame) in frames.iter().enumerate() {
        let at = format!("frames[{index}]");
        let frame = array(frame, &at)?;
        if frame.len() != 2 {
            return Err(refused(RefusalReason::Type, at));
        }
        if matches!((&frame[0], &frame[1]), (CborValue::Null, CborValue::Null)) {
            continue;
        }
        let CborValue::Bytes(build_id) = &frame[0] else {
            return Err(refused(RefusalReason::Type, at));
        };
        let offset = uint(&frame[1], &at)?;
        if !(8..=32).contains(&build_id.len()) {
            return Err(refused(RefusalReason::Grammar, format!("{at}.build_id")));
        }
        if offset > MAX_OFFSET {
            return Err(refused(RefusalReason::Range, format!("{at}.offset")));
        }
    }
    Ok(())
}

fn validate_samples(value: &CborValue) -> Result<()> {
    let samples = array(value, "samples")?;
    if samples.len() > MAX_SAMPLES {
        return Err(refused(RefusalReason::Range, "samples"));
    }
    for (index, count) in samples.iter().enumerate() {
        let at = format!("samples[{index}]");
        if !(1..=MAX_FRAMES as u64).contains(&uint(count, &at)?) {
            return Err(refused(RefusalReason::Range, at));
        }
    }
    Ok(())
}

fn valid_word(value: &str, max: usize) -> bool {
    (1..=max).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn validate_kframes(value: &CborValue) -> Result<()> {
    let frames = array(value, "kframes")?;
    if frames.len() > MAX_KFRAMES {
        return Err(refused(RefusalReason::Range, "kframes"));
    }
    for (index, frame) in frames.iter().enumerate() {
        let at = format!("kframes[{index}]");
        let frame = array(frame, &at)?;
        if frame.len() != 4 {
            return Err(refused(RefusalReason::Type, at));
        }
        ascii_grammar(&frame[0], &format!("{at}.sym"), 48, |value| {
            valid_word(value, 48)
        })?;
        let offset = uint(&frame[1], &at)?;
        let size = uint(&frame[2], &at)?;
        if offset > MAX_KOFFSET || size > MAX_KOFFSET {
            return Err(refused(RefusalReason::Range, at));
        }
        if !matches!(frame[3], CborValue::Null) {
            ascii_grammar(&frame[3], &format!("{at}.module"), 24, |value| {
                valid_word(value, 24)
            })?;
        }
    }
    Ok(())
}

fn presence(kind: &str, key: &str) -> Option<bool> {
    match key {
        "v" | "kind" | "how" | "day" | "build" | "model" | "uptime" | "del" => Some(true),
        "app" | "session" | "mem" | "frames" => Some(kind != "kernel"),
        "samples" => Some(kind == "hang"),
        "kframes" => Some(kind == "kernel"),
        "panic_at" if matches!(kind, "crash" | "component") => None,
        "panic_at" => Some(false),
        _ => unreachable!(),
    }
}

fn how_allowed(kind: &str, how: &str) -> bool {
    match kind {
        "crash" => SIGNALS.contains(&how),
        "hang" => matches!(how, "no-reply-menu" | "stop-timeout"),
        "oom" => how == "oom-kill",
        "component" => {
            SIGNALS.contains(&how)
                || matches!(how, "start-timeout" | "stop-timeout" | "no-reply-menu")
        }
        "kernel" => matches!(how, "oops" | "panic" | "watchdog"),
        _ => false,
    }
}

fn validate(value: &CborValue) -> Result<()> {
    let entries = value.fields()?;
    for (key, _) in entries {
        if !TOP_LEVEL_FIELDS.contains(&key.as_str()) {
            return Err(refused(RefusalReason::UnknownField, key));
        }
    }
    let version = field(entries, "v").ok_or_else(|| refused(RefusalReason::MissingField, "v"))?;
    if uint(version, "v")? != 1 {
        return Err(refused(RefusalReason::Version, "v"));
    }
    let kind = field(entries, "kind")
        .ok_or_else(|| refused(RefusalReason::MissingField, "kind"))?
        .text("kind")?;
    if !KINDS.contains(&kind) {
        return Err(refused(RefusalReason::Vocabulary, "kind"));
    }
    let mut ordered_fields = TOP_LEVEL_FIELDS.to_vec();
    ordered_fields.sort_by_key(|key| (key.len(), *key));
    for key in ordered_fields {
        match (presence(kind, key), field(entries, key).is_some()) {
            (Some(true), false) => return Err(refused(RefusalReason::MissingField, key)),
            (Some(false), true) => return Err(refused(RefusalReason::ForbiddenField, key)),
            _ => {}
        }
    }

    let mut ordered_entries: Vec<_> = entries.iter().collect();
    ordered_entries.sort_by_key(|(key, _)| (key.len(), key.as_str()));
    for (key, value) in ordered_entries {
        match key.as_str() {
            "app" => validate_app(value)?,
            "day" => ascii_grammar(value, "day", 10, valid_day)?,
            "del" => match value {
                CborValue::Bytes(bytes) if bytes.len() == 32 => {}
                CborValue::Bytes(_) => return Err(refused(RefusalReason::Grammar, "del")),
                _ => return Err(refused(RefusalReason::Type, "del")),
            },
            "how" => {
                if !how_allowed(kind, value.text("how")?) {
                    return Err(refused(RefusalReason::Vocabulary, "how"));
                }
            }
            "mem" => validate_mem(value)?,
            "build" => ascii_grammar(value, "build", 12, |text| {
                text.len() == 12
                    && text
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })?,
            "model" => ascii_grammar(value, "model", 32, valid_model)?,
            "frames" => validate_frames(value)?,
            "uptime" | "session" => {
                if !BUCKETS.contains(&value.text(key)?) {
                    return Err(refused(RefusalReason::Vocabulary, key));
                }
            }
            "kframes" => validate_kframes(value)?,
            "samples" => validate_samples(value)?,
            "panic_at" => ascii_grammar(value, "panic_at", 128, valid_panic_at)?,
            "v" | "kind" => {}
            _ => unreachable!("unknown fields rejected above"),
        }
    }

    if kind == "component"
        && value
            .field("app")
            .and_then(|app| app.field("origin"))
            .and_then(|origin| origin.text("app.origin").ok())
            != Some("image")
    {
        return Err(refused(RefusalReason::Inconsistent, "app.origin"));
    }
    if kind == "oom"
        && !boolean(
            value
                .field("mem")
                .and_then(|mem| mem.field("oom"))
                .expect("required validated"),
            "mem.oom",
        )?
    {
        return Err(refused(RefusalReason::Inconsistent, "mem.oom"));
    }
    if kind == "hang" {
        let sum: u64 = value
            .field("samples")
            .expect("required validated")
            .array("samples")?
            .iter()
            .map(|count| uint(count, "samples").expect("validated sample"))
            .sum();
        let frame_count = value
            .field("frames")
            .expect("required validated")
            .array("frames")?
            .len() as u64;
        if sum != frame_count {
            return Err(refused(RefusalReason::Inconsistent, "samples"));
        }
    }
    if value.field("panic_at").is_some() {
        let origin = value
            .field("app")
            .and_then(|app| app.field("origin"))
            .and_then(|origin| origin.text("app.origin").ok());
        let how = value.field("how").and_then(|how| how.text("how").ok());
        if origin != Some("image") || how != Some("SIGABRT") {
            return Err(refused(RefusalReason::Inconsistent, "panic_at"));
        }
    }
    Ok(())
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if value.len() % 2 != 0
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    Some(
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let high = if pair[0].is_ascii_digit() {
                    pair[0] - b'0'
                } else {
                    pair[0] - b'a' + 10
                };
                let low = if pair[1].is_ascii_digit() {
                    pair[1] - b'0'
                } else {
                    pair[1] - b'a' + 10
                };
                (high << 4) | low
            })
            .collect(),
    )
}

fn charge_json_budget(budget: &mut usize, amount: usize) -> Result<()> {
    *budget = budget
        .checked_add(amount)
        .ok_or_else(|| refused(RefusalReason::TooLarge, ""))?;
    if *budget > MAX_BYTES {
        return Err(refused(RefusalReason::TooLarge, ""));
    }
    Ok(())
}

fn preflight_json(value: &JsonValue, depth: usize, budget: &mut usize) -> Result<()> {
    // One byte is the minimum encoded representation of every JSON value. It
    // also bounds traversal of containers containing zero-length strings.
    charge_json_budget(budget, 1)?;
    match value {
        JsonValue::String(value) => charge_json_budget(budget, value.len()),
        JsonValue::Array(values) => {
            if depth >= MAX_DEPTH {
                return Err(refused(RefusalReason::Unsupported, "nesting"));
            }
            for value in values {
                preflight_json(value, depth + 1, budget)?;
            }
            Ok(())
        }
        JsonValue::Object(entries) => {
            if depth >= MAX_DEPTH {
                return Err(refused(RefusalReason::Unsupported, "nesting"));
            }
            for (key, value) in entries {
                charge_json_budget(budget, key.len())?;
                charge_json_budget(budget, 1)?;
                preflight_json(value, depth + 1, budget)?;
            }
            Ok(())
        }
        JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) => Ok(()),
    }
}

fn json_to_cbor(value: &JsonValue) -> Result<CborValue> {
    match value {
        JsonValue::Null => Ok(CborValue::Null),
        JsonValue::Bool(value) => Ok(CborValue::Bool(*value)),
        JsonValue::Number(value) => value
            .as_u64()
            .map(CborValue::Uint)
            .ok_or_else(|| refused(RefusalReason::Unsupported, "number")),
        JsonValue::String(value) => Ok(CborValue::Text(value.clone())),
        JsonValue::Array(values) => values
            .iter()
            .map(json_to_cbor)
            .collect::<Result<Vec<_>>>()
            .map(CborValue::Array),
        JsonValue::Object(entries) => entries
            .iter()
            .map(|(key, value)| Ok((key.clone(), json_to_cbor(value)?)))
            .collect::<Result<Vec<_>>>()
            .map(CborValue::Map),
    }
}

fn report_json_to_cbor(report: &JsonValue) -> Result<CborValue> {
    let mut value = json_to_cbor(report)?;
    let CborValue::Map(entries) = &mut value else {
        return Ok(value);
    };
    if let Some((_, del)) = entries.iter_mut().find(|(key, _)| key == "del") {
        if let CborValue::Text(text) = del {
            if let Some(bytes) = decode_hex(text) {
                *del = CborValue::Bytes(bytes);
            }
        }
    }
    if let Some(CborValue::Array(frames)) = entries
        .iter_mut()
        .find(|(key, _)| key == "frames")
        .map(|(_, value)| value)
    {
        for frame in frames {
            let CborValue::Array(parts) = frame else {
                continue;
            };
            if let Some(CborValue::Text(text)) = parts.first() {
                if let Some(bytes) = decode_hex(text) {
                    parts[0] = CborValue::Bytes(bytes);
                }
            }
        }
    }
    Ok(value)
}

fn cbor_to_json(value: &CborValue) -> JsonValue {
    match value {
        CborValue::Null => JsonValue::Null,
        CborValue::Bool(value) => JsonValue::Bool(*value),
        CborValue::Uint(value) => JsonValue::Number(JsonNumber::from(*value)),
        CborValue::Bytes(value) => {
            JsonValue::String(value.iter().map(|byte| format!("{byte:02x}")).collect())
        }
        CborValue::Text(value) => JsonValue::String(value.clone()),
        CborValue::Array(values) => JsonValue::Array(values.iter().map(cbor_to_json).collect()),
        CborValue::Map(entries) => {
            let mut object = JsonMap::new();
            for (key, value) in entries {
                object.insert(key.clone(), cbor_to_json(value));
            }
            JsonValue::Object(object)
        }
    }
}

/// Decode and validate one complete canonical problem report.
pub fn decode(data: &[u8]) -> Result<JsonValue> {
    if data.len() > MAX_BYTES {
        return Err(refused(RefusalReason::TooLarge, ""));
    }
    let value = Parser::new(data).parse()?;
    validate(&value)?;
    Ok(cbor_to_json(&value))
}

/// Validate the JSON mapping and encode its deterministic CBOR representation.
pub fn encode(report: &JsonValue) -> Result<Vec<u8>> {
    let mut budget = 0;
    preflight_json(report, 0, &mut budget)?;
    let value = report_json_to_cbor(report)?;
    let mut data = Vec::new();
    encode_value(&value, &mut data)?;
    let decoded = decode(&data)?;
    if decoded != *report {
        return Err(refused(
            RefusalReason::Type,
            "JSON mapping does not round-trip",
        ));
    }
    Ok(data)
}
