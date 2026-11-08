//! Load the image-verified descriptor and apply W3's own `[fetch]` checks on top of
//! pf-app-manifest's (which do not validate the URL scheme or `format`, and do not yet
//! reserve `.pf-fetch`). Every refusal is `descriptor_invalid`, before any GET.

use crate::codes::{Code, Failure, Result};
use crate::scope::{parse_https, Url};
use pf_app_manifest::{parse_manifest, Fetch, FetchSource};
use std::collections::BTreeSet;
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Format {
    TarGz,
    Tar,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub url: Url,
    pub sha256: String,
    pub size: u64,
    pub format: Format,
    pub dest: String,
    pub strip: u64,
    pub include: Option<Vec<String>>,
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub key: String,
    pub entries: Vec<Entry>,
}

fn invalid(detail: impl Into<String>) -> Failure {
    Failure::new(Code::DescriptorInvalid, detail)
}

pub fn load(path: &Path, app_id: &str) -> Result<Plan> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| invalid(format!("read {}: {e}", path.display())))?;
    let manifest = parse_manifest(&text).map_err(|e| invalid(e.to_string()))?;
    if manifest.app.id != app_id {
        return Err(invalid(format!(
            "descriptor app.id {} is not {app_id}",
            manifest.app.id
        )));
    }
    let fetch = manifest
        .fetch
        .ok_or_else(|| invalid("descriptor has no [fetch] block"))?;
    plan(&fetch)
}

pub fn plan(fetch: &Fetch) -> Result<Plan> {
    if !fetch.enabled {
        return Err(invalid("[fetch] is not enabled"));
    }
    if fetch.source.unwrap_or_default() != FetchSource::Pinned {
        return Err(invalid("only source = \"pinned\" is supported"));
    }
    // destination: absent or "var" both mean the per-app state root (tsp-ziac.4.3.3 owns
    // any other value).
    match fetch.destination.as_deref() {
        None | Some("var") => {}
        Some(other) => return Err(invalid(format!("unsupported destination {other}"))),
    }
    let files = fetch.files.as_deref().unwrap_or_default();
    if files.is_empty() {
        return Err(invalid("[fetch] has no files"));
    }
    let mut dests = BTreeSet::new();
    let mut entries = Vec::new();
    for file in files {
        if file.executable.is_some() {
            return Err(invalid(
                "the executable map is refused: pf-app-fetch installs data only",
            ));
        }
        let url = parse_https(&file.url).map_err(invalid)?;
        let size = file.size.ok_or_else(|| invalid("file without size"))?;
        let format = match file.format.as_deref() {
            Some("tar.gz") => Format::TarGz,
            Some("tar") => Format::Tar,
            other => return Err(invalid(format!("unsupported format {other:?}"))),
        };
        let dest = file
            .dest
            .clone()
            .ok_or_else(|| invalid("file without dest"))?;
        if dest.is_empty()
            || dest.contains('/')
            || dest.starts_with('.')
            || matches!(dest.as_str(), "config" | "state" | "cache")
        {
            return Err(invalid(format!("reserved or invalid dest {dest:?}")));
        }
        if !dests.insert(dest.clone()) {
            return Err(invalid(format!("two files share dest {dest:?}")));
        }
        entries.push(Entry {
            url,
            sha256: file.sha256.clone(),
            size,
            format,
            dest,
            strip: file.strip_components.unwrap_or(0),
            include: file.include.clone(),
        });
    }
    let key = pf_app_manifest::fetch_receipt_key(fetch)
        .ok_or_else(|| invalid("no receipt key for an enabled block"))?;
    Ok(Plan { key, entries })
}
