//! Verified-archive extraction (§4.6 items 2, 3 and 5).
//!
//! Only `tar::Archive::entries` is used, never `unpack`: every member is classified here
//! and only regular files are written, through `O_CREAT|O_EXCL|O_NOFOLLOW`, as 0644 files
//! in 0755 directories. Directories are created only as parents of files, so the payload
//! is exactly what the receipt manifest lists.
//!
//! Order per member: `include` matches the member name BEFORE `strip_components` (item 5);
//! then `strip_components`; then the refusals. An absolute or `..` name is refused anywhere
//! in the archive; links, devices, FIFOs and duplicates are refused when selected (release
//! tarballs carry library symlinks outside the selected subtree).

use crate::codes::{io_failure, Code, Failure, Result};
use crate::descriptor::{Entry, Format};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

pub const MAX_MEMBERS: u64 = 200_000;
pub const MAX_EXPANDED: u64 = 4 << 30;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

fn unsafe_member(detail: impl Into<String>) -> Failure {
    Failure::new(Code::ArchiveMemberUnsafe, detail)
}

fn invalid(detail: impl Into<String>) -> Failure {
    Failure::new(Code::ArchiveInvalid, detail)
}

/// The expansion bound used for the pre-extract free-space check: the gzip ISIZE trailer
/// (uncompressed size mod 2^32) or the plain tar's own length.
pub fn expanded_bound(archive: &Path, format: Format) -> Result<u64> {
    let len = fs::metadata(archive)
        .map_err(|e| invalid(format!("stat archive: {e}")))?
        .len();
    match format {
        Format::Tar => Ok(len),
        Format::TarGz => {
            if len < 18 {
                return Err(invalid("gzip stream too short"));
            }
            let mut file = File::open(archive).map_err(|e| invalid(format!("open: {e}")))?;
            use std::io::{Seek, SeekFrom};
            // Trust the trailer only for a gzip stream (magic 1f 8b, deflate). Anything else
            // is archive_invalid here, never a misleading disk_full from garbage bytes.
            let mut magic = [0u8; 3];
            file.read_exact(&mut magic)
                .map_err(|e| invalid(format!("read gzip header: {e}")))?;
            if magic != [0x1f, 0x8b, 0x08] {
                return Err(invalid("not a gzip (deflate) stream"));
            }
            file.seek(SeekFrom::End(-4))
                .map_err(|e| invalid(format!("seek: {e}")))?;
            let mut trailer = [0u8; 4];
            file.read_exact(&mut trailer)
                .map_err(|e| invalid(format!("read ISIZE: {e}")))?;
            Ok(u64::from(u32::from_le_bytes(trailer)))
        }
    }
}

/// Components of a member name with `.` and empty components dropped. `Err` for an
/// absolute name or any `..` component.
fn components(name: &str) -> std::result::Result<Vec<&str>, String> {
    if name.starts_with('/') {
        return Err(format!("absolute member {name:?}"));
    }
    let mut out = Vec::new();
    for part in name.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(format!("member {name:?} has a .. component")),
            other => out.push(other),
        }
    }
    Ok(out)
}

fn selected(name_parts: &[&str], is_dir: bool, include: Option<&[String]>) -> bool {
    let Some(include) = include else {
        return true;
    };
    let joined = name_parts.join("/");
    include.iter().any(|prefix| match prefix.strip_suffix('/') {
        Some(dir) => joined == dir && is_dir || joined.starts_with(&format!("{dir}/")),
        None => !is_dir && joined == *prefix,
    })
}

fn create_dirs(root: &Path, parts: &[&str], dirs: &mut BTreeSet<String>) -> Result<()> {
    for depth in 1..=parts.len() {
        let rel = parts[..depth].join("/");
        if dirs.contains(&rel) {
            continue;
        }
        let path = root.join(&rel);
        fs::DirBuilder::new()
            .mode(0o755)
            .create(&path)
            .map_err(|e| io_failure(Code::StateInvalid, "create directory", &e))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .map_err(|e| io_failure(Code::StateInvalid, "chmod directory", &e))?;
        dirs.insert(rel);
    }
    Ok(())
}

fn fsync_dir(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|d| d.sync_all())
        .map_err(|e| io_failure(Code::StateInvalid, "fsync directory", &e))
}

/// Extract the verified archive at `archive` into the new, empty directory `root`.
/// Returns the sorted payload manifest (paths relative to `root`).
pub fn extract(
    archive: &Path,
    entry: &Entry,
    root: &Path,
    cancel: &AtomicBool,
) -> Result<Vec<ManifestEntry>> {
    let file = File::open(archive).map_err(|e| invalid(format!("open archive: {e}")))?;
    let reader: Box<dyn Read> = match entry.format {
        Format::TarGz => Box::new(flate2::read::GzDecoder::new(std::io::BufReader::new(file))),
        Format::Tar => Box::new(std::io::BufReader::new(file)),
    };
    let mut tar = tar::Archive::new(reader);
    let include = entry.include.as_deref();
    let strip = usize::try_from(entry.strip).unwrap_or(usize::MAX);
    let mut files: BTreeMap<String, ManifestEntry> = BTreeMap::new();
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    let mut seen_dirs: BTreeSet<String> = BTreeSet::new();
    let mut members: u64 = 0;
    let mut expanded: u64 = 0;
    let mut written_dirs: BTreeSet<String> = BTreeSet::new();
    let entries = tar
        .entries()
        .map_err(|e| invalid(format!("read archive: {e}")))?;
    for member in entries {
        if cancel.load(Ordering::SeqCst) {
            return Err(Failure::new(Code::Cancelled, "stop requested"));
        }
        let mut member = member.map_err(|e| invalid(format!("read member: {e}")))?;
        members += 1;
        if members > MAX_MEMBERS {
            return Err(invalid(format!("more than {MAX_MEMBERS} members")));
        }
        let kind = member.header().entry_type();
        if kind.is_pax_global_extensions() {
            continue;
        }
        let raw = member.path_bytes().into_owned();
        let name = String::from_utf8(raw).map_err(|_| invalid("member name is not UTF-8"))?;
        let parts = components(&name).map_err(unsafe_member)?;
        let is_dir = kind.is_dir();
        if !selected(&parts, is_dir, include) {
            continue;
        }
        if parts.len() <= strip {
            if is_dir {
                continue;
            }
            // A selected non-directory that strip_components removes entirely.
            if kind.is_file() {
                continue;
            }
        }
        let stripped: Vec<&str> = parts.iter().skip(strip).copied().collect();
        let rel = stripped.join("/");
        if kind.is_symlink() || kind.is_hard_link() {
            return Err(unsafe_member(format!("link member {name:?}")));
        }
        if kind.is_character_special() || kind.is_block_special() || kind.is_fifo() {
            return Err(unsafe_member(format!("device or FIFO member {name:?}")));
        }
        if is_dir {
            if files.contains_key(&rel) {
                return Err(unsafe_member(format!("directory {name:?} shadows a file")));
            }
            seen_dirs.insert(rel);
            continue;
        }
        if !(kind.is_file() || kind == tar::EntryType::Continuous) {
            return Err(invalid(format!(
                "unsupported member type {:?} for {name:?}",
                kind.as_byte() as char
            )));
        }
        if rel.is_empty() {
            continue;
        }
        if files.contains_key(&rel) || seen_dirs.contains(&rel) || dirs.contains(&rel) {
            return Err(unsafe_member(format!("duplicate member {name:?}")));
        }
        for depth in 1..stripped.len() {
            if files.contains_key(&stripped[..depth].join("/")) {
                return Err(unsafe_member(format!("member {name:?} is below a file")));
            }
        }
        let size = member.header().size().map_err(|e| invalid(e.to_string()))?;
        expanded = expanded.saturating_add(size);
        if expanded > MAX_EXPANDED {
            return Err(invalid("archive expands beyond 4 GiB"));
        }
        let parent = &stripped[..stripped.len() - 1];
        create_dirs(root, parent, &mut dirs)?;
        for depth in 1..=parent.len() {
            written_dirs.insert(parent[..depth].join("/"));
        }
        let path = root.join(&rel);
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|e| io_failure(Code::StateInvalid, "create payload file", &e))?;
        let mut hasher = Sha256::new();
        let mut copied: u64 = 0;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = member
                .read(&mut buf)
                .map_err(|e| invalid(format!("read {name:?}: {e}")))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])
                .map_err(|e| io_failure(Code::StateInvalid, "write payload file", &e))?;
            copied += n as u64;
        }
        if copied != size {
            return Err(invalid(format!("member {name:?} is short")));
        }
        out.set_permissions(fs::Permissions::from_mode(0o644))
            .map_err(|e| io_failure(Code::StateInvalid, "chmod payload file", &e))?;
        out.sync_all()
            .map_err(|e| io_failure(Code::StateInvalid, "fsync payload file", &e))?;
        files.insert(
            rel.clone(),
            ManifestEntry {
                path: rel,
                size,
                sha256: hasher
                    .finalize()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect(),
            },
        );
    }
    if files.is_empty() {
        return Err(invalid("strip_components and include leave no files"));
    }
    for dir in written_dirs.iter().rev() {
        fsync_dir(&root.join(dir))?;
    }
    fsync_dir(root)?;
    Ok(files.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expanded_bound_refuses_non_gzip_before_reading_a_trailer() {
        let dir = std::env::temp_dir().join(format!("pf-app-fetch-bound-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a");
        // Garbage whose last four bytes would claim a ~1.9 GB expansion.
        fs::write(&path, b"definitely not gzip").unwrap();
        let err = expanded_bound(&path, Format::TarGz).unwrap_err();
        assert_eq!(err.code, Code::ArchiveInvalid);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&[0u8; 5000]).unwrap();
        fs::write(&path, gz.finish().unwrap()).unwrap();
        assert_eq!(expanded_bound(&path, Format::TarGz).unwrap(), 5000);
        fs::remove_dir_all(&dir).unwrap();
    }
}
