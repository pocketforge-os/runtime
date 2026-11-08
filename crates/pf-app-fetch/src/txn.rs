//! The run transaction (§4.6 item 3 as amended by W3, tsp-ziac.4.3.5 design item 3):
//!
//! 1. sweep `.staging.*`, `.old.*` and receipt temp files left by a killed run;
//! 2. check: a valid receipt whose full SHA256 re-check passes stops here, zero GETs;
//! 3. invalidate: unlink the receipt, fsync `.pf-fetch/` (NeedsSetup from here);
//! 4. move each existing `<dest>` into `.old.*`, fsync, delete (frees space BEFORE staging:
//!    a replacing run only ever replaces a payload that is already NeedsSetup);
//! 5. stage: free-space precheck, download, size, SHA256, ISIZE precheck, extract, delete
//!    the archive, fsync;
//! 6. rename each staged tree onto the now-absent `<dest>`, fsync;
//! 7. commit: temp receipt, fsync, rename, fsync; then delete the staging directory.
//!
//! A kill anywhere leaves NeedsSetup or the untouched pre-run state (which was itself never
//! Ready when a replace began). Receipt present implies its payload is in place.

use crate::codes::{io_failure, Code, Failure, Result};
use crate::confine::{self, Mode};
use crate::descriptor::{self, Plan};
use crate::extract::{self, ManifestEntry};
use crate::hooks;
use crate::http::{self, Client};
use crate::scope::{self, Scope};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub const RECEIPT_SCHEMA: u64 = 1;
/// Free space kept for the rest of the system on top of every precheck.
pub const FREE_FLOOR: u64 = 32 << 20;

pub struct Config {
    pub app_id: String,
    pub descriptor: PathBuf,
    pub state_dir: PathBuf,
    pub ca_file: PathBuf,
    pub confine: Mode,
}

#[derive(Debug, Default)]
pub struct Report {
    /// The step-2 verdict: `valid`, `absent`, `receipt_stale` or `state_invalid`.
    pub check: &'static str,
    pub gets: usize,
    /// The Landlock ABI enforced on this run (None: not applied, test builds only).
    pub landlock: Option<u32>,
}

fn state_invalid(detail: impl Into<String>) -> Failure {
    Failure::new(Code::StateInvalid, detail)
}

pub fn random_hex() -> String {
    let mut bytes = [0u8; 8];
    // SAFETY: getrandom(2) writes at most `bytes.len()` bytes into the live buffer.
    let n = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
    if n != bytes.len() as isize {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        bytes = ((nanos as u64) ^ u64::from(std::process::id())).to_le_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn fsync_dir(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|d| d.sync_all())
        .map_err(|e| io_failure(Code::StateInvalid, &format!("fsync {}", path.display()), &e))
}

fn remove_any(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn free_bytes(path: &Path) -> Result<u64> {
    if let Some(free) = hooks::free_bytes() {
        return Ok(free);
    }
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| state_invalid("state path has a NUL"))?;
    // SAFETY: statvfs(3) fills the zeroed struct for a valid NUL-terminated path.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut st) } != 0 {
        return Err(state_invalid(format!(
            "statvfs: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

fn require_space(path: &Path, need: u64, what: &str) -> Result<()> {
    let free = free_bytes(path)?;
    let need = need.saturating_add(FREE_FLOOR);
    if free < need {
        return Err(Failure::new(
            Code::DiskFull,
            format!("{what}: {free} bytes free, {need} needed"),
        ));
    }
    Ok(())
}

fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::SeqCst) {
        Err(Failure::new(Code::Cancelled, "stop requested"))
    } else {
        Ok(())
    }
}

/// Step 2: the full check of the receipt and payload.
pub fn check(state: &Path, fetch_dir: &Path, plan: &Plan) -> &'static str {
    let text = match fs::read_to_string(fetch_dir.join("receipt.json")) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return "absent",
        Err(_) => return "state_invalid",
    };
    let Ok(receipt) = serde_json::from_str::<Value>(&text) else {
        return "state_invalid";
    };
    if receipt["schema_version"].as_u64() != Some(RECEIPT_SCHEMA) {
        return "state_invalid";
    }
    if receipt["key"].as_str() != Some(plan.key.as_str()) {
        return "receipt_stale";
    }
    let Some(files) = receipt["files"].as_array() else {
        return "state_invalid";
    };
    if files.len() != plan.entries.len() {
        return "state_invalid";
    }
    for (entry, record) in plan.entries.iter().zip(files) {
        if record["dest"].as_str() != Some(entry.dest.as_str())
            || !payload_matches(&state.join(&entry.dest), record)
        {
            return "state_invalid";
        }
    }
    "valid"
}

fn payload_matches(dir: &Path, record: &Value) -> bool {
    let Some(manifest) = record["manifest"].as_array() else {
        return false;
    };
    let mut want: BTreeMap<String, (u64, String)> = BTreeMap::new();
    for item in manifest {
        let (Some(path), Some(size), Some(sha)) = (
            item["path"].as_str(),
            item["size"].as_u64(),
            item["sha256"].as_str(),
        ) else {
            return false;
        };
        want.insert(path.to_string(), (size, sha.to_string()));
    }
    let total: u64 = want.values().map(|(size, _)| size).sum();
    if record["file_count"].as_u64() != Some(want.len() as u64)
        || record["total_bytes"].as_u64() != Some(total)
    {
        return false;
    }
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        _ => return false,
    }
    let mut found = BTreeMap::new();
    let mut dirs = BTreeSet::new();
    if !walk(dir, "", &mut found, &mut dirs) {
        return false;
    }
    if found.keys().ne(want.keys()) {
        return false;
    }
    // Every directory must be the parent of a listed file (no extra entries at all).
    let parents: BTreeSet<String> = want
        .keys()
        .flat_map(|path| {
            let parts: Vec<&str> = path.split('/').collect();
            (1..parts.len())
                .map(|depth| parts[..depth].join("/"))
                .collect::<Vec<_>>()
        })
        .collect();
    if dirs != parents {
        return false;
    }
    for (path, (size, sha)) in &want {
        if found.get(path) != Some(size) || !sha_matches(&dir.join(path), sha) {
            return false;
        }
    }
    true
}

fn walk(
    dir: &Path,
    prefix: &str,
    files: &mut BTreeMap<String, u64>,
    dirs: &mut BTreeSet<String>,
) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries {
        let Ok(entry) = entry else { return false };
        let Ok(name) = entry.file_name().into_string() else {
            return false;
        };
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            return false;
        };
        if meta.is_dir() {
            dirs.insert(rel.clone());
            if !walk(&entry.path(), &rel, files, dirs) {
                return false;
            }
        } else if meta.is_file() {
            files.insert(rel, meta.len());
        } else {
            return false;
        }
    }
    true
}

fn sha_matches(path: &Path, want: &str) -> bool {
    let Ok(mut file) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    else {
        return false;
    };
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return false,
        }
    }
    let got: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    got == want
}

fn prepare_fetch_dir(state: &Path) -> Result<PathBuf> {
    match fs::symlink_metadata(state) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(state_invalid("app state path is not a directory")),
        Err(e) => return Err(state_invalid(format!("app state directory: {e}"))),
    }
    let fetch_dir = state.join(".pf-fetch");
    match fs::symlink_metadata(&fetch_dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(state_invalid(".pf-fetch is not a directory")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .mode(0o755)
                .create(&fetch_dir)
                .map_err(|e| io_failure(Code::StateInvalid, "create .pf-fetch", &e))?;
        }
        Err(e) => return Err(state_invalid(format!(".pf-fetch: {e}"))),
    }
    fs::set_permissions(&fetch_dir, fs::Permissions::from_mode(0o755))
        .map_err(|e| io_failure(Code::StateInvalid, "chmod .pf-fetch", &e))?;
    Ok(fetch_dir)
}

fn lock(fetch_dir: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(fetch_dir.join("lock"))
        .map_err(|e| io_failure(Code::StateInvalid, "open lock", &e))?;
    // SAFETY: flock(2) on a descriptor we own.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(state_invalid("another pf-app-fetch run holds the lock"));
    }
    Ok(file)
}

fn sweep(fetch_dir: &Path) -> Result<()> {
    let entries = fs::read_dir(fetch_dir)
        .map_err(|e| io_failure(Code::StateInvalid, "read .pf-fetch", &e))?;
    let mut removed = false;
    for entry in entries {
        let entry = entry.map_err(|e| io_failure(Code::StateInvalid, "read .pf-fetch", &e))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".staging.")
            || name.starts_with(".old.")
            || name.starts_with(".receipt.json.")
            || name.starts_with(".result.json.")
        {
            remove_any(&entry.path()).map_err(|e| io_failure(Code::StateInvalid, "sweep", &e))?;
            removed = true;
        }
    }
    if removed {
        fsync_dir(fetch_dir)?;
    }
    Ok(())
}

/// Write `contents` to `dir/name` atomically as a 0644 file (temp, fsync, rename, fsync).
pub fn write_atomic(dir: &Path, name: &str, contents: &[u8]) -> Result<()> {
    let tmp = dir.join(format!(".{name}.{}", random_hex()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&tmp)?;
        file.set_permissions(fs::Permissions::from_mode(0o644))?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&tmp, dir.join(name))
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(io_failure(Code::StateInvalid, &format!("write {name}"), &e));
    }
    fsync_dir(dir)
}

struct Staged {
    dest: String,
    archive_sha256: String,
    manifest: Vec<ManifestEntry>,
}

fn stage(
    plan: &Plan,
    client: &mut Client,
    state: &Path,
    staging: &Path,
    cancel: &AtomicBool,
) -> Result<Vec<Staged>> {
    let mut staged = Vec::new();
    for (index, entry) in plan.entries.iter().enumerate() {
        cancelled(cancel)?;
        let archive = staging.join(format!("archive.{index}"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&archive)
            .map_err(|e| io_failure(Code::StateInvalid, "create archive", &e))?;
        client.download(entry, &mut file, cancel)?;
        drop(file);
        let bound = extract::expanded_bound(&archive, entry.format)?;
        require_space(state, bound, "before extracting")?;
        let tree = staging.join(format!("tree.{index}"));
        fs::DirBuilder::new()
            .mode(0o755)
            .create(&tree)
            .and_then(|_| fs::set_permissions(&tree, fs::Permissions::from_mode(0o755)))
            .map_err(|e| io_failure(Code::StateInvalid, "create staged tree", &e))?;
        let manifest = extract::extract(&archive, entry, &tree, cancel)?;
        fs::remove_file(&archive)
            .map_err(|e| io_failure(Code::StateInvalid, "delete archive", &e))?;
        staged.push(Staged {
            dest: entry.dest.clone(),
            archive_sha256: entry.sha256.clone(),
            manifest,
        });
    }
    fsync_dir(staging)?;
    Ok(staged)
}

fn receipt_json(plan: &Plan, staged: &[Staged]) -> Vec<u8> {
    let files: Vec<Value> = staged
        .iter()
        .map(|s| {
            json!({
                "dest": s.dest,
                "archive_sha256": s.archive_sha256,
                "file_count": s.manifest.len(),
                "total_bytes": s.manifest.iter().map(|m| m.size).sum::<u64>(),
                "manifest": s.manifest.iter().map(|m| json!({
                    "path": m.path,
                    "size": m.size,
                    "sha256": m.sha256,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut text = serde_json::to_vec(&json!({
        "schema_version": RECEIPT_SCHEMA,
        "key": plan.key,
        "files": files,
    }))
    .expect("receipt serializes");
    text.push(b'\n');
    text
}

/// One run. `report` is filled as far as the run got, for result.json.
pub fn run(cfg: &Config, cancel: &AtomicBool, report: &mut Report) -> Result<()> {
    let plan = descriptor::load(&cfg.descriptor, &cfg.app_id)?;
    let roots = http::load_roots(&cfg.ca_file)?;
    let state = cfg.state_dir.as_path();
    let fetch_dir = prepare_fetch_dir(state)?;
    let _lock = lock(&fetch_dir)?;

    let table = hooks::redirect_table().unwrap_or_else(scope::platform_table);
    let first_hops: Vec<_> = plan.entries.iter().map(|e| e.url.clone()).collect();
    let scope = Scope::new(&first_hops, &table);
    let overrides = hooks::resolve_overrides();
    let mut ports = scope.ports();
    ports.extend(overrides.values().map(|addr| addr.port()));
    report.landlock = confine::apply(cfg.confine, state, &ports).map_err(state_invalid)?;

    // 1. Sweep.
    sweep(&fetch_dir)?;
    // 2. Check.
    report.check = check(state, &fetch_dir, &plan);
    if report.check == "valid" {
        return Ok(());
    }
    cancelled(cancel)?;
    // 3. Invalidate.
    match fs::remove_file(fetch_dir.join("receipt.json")) {
        Ok(()) => fsync_dir(&fetch_dir)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_failure(Code::StateInvalid, "unlink receipt", &e)),
    }
    hooks::stop_at("after-invalidate");
    // 4. Move every existing <dest> aside, then delete it.
    let old = fetch_dir.join(format!(".old.{}", random_hex()));
    let mut moved = false;
    for entry in &plan.entries {
        let dest = state.join(&entry.dest);
        if fs::symlink_metadata(&dest).is_ok() {
            if !moved {
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&old)
                    .map_err(|e| io_failure(Code::StateInvalid, "create .old", &e))?;
                moved = true;
            }
            fs::rename(&dest, old.join(&entry.dest))
                .map_err(|e| io_failure(Code::StateInvalid, "move old payload aside", &e))?;
        }
    }
    if moved {
        fsync_dir(state)?;
        fsync_dir(&fetch_dir)?;
    }
    hooks::stop_at("after-move-aside");
    if moved {
        fs::remove_dir_all(&old)
            .map_err(|e| io_failure(Code::StateInvalid, "delete old payload", &e))?;
        fsync_dir(&fetch_dir)?;
    }
    cancelled(cancel)?;
    // 5. Stage.
    let total: u64 = plan.entries.iter().map(|e| e.size).sum();
    require_space(state, total, "before downloading")?;
    let staging = fetch_dir.join(format!(".staging.{}", random_hex()));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&staging)
        .map_err(|e| io_failure(Code::StateInvalid, "create staging", &e))?;
    let mut client = http::Client::new(roots, scope, overrides);
    let staged = stage(&plan, &mut client, state, &staging, cancel);
    report.gets = client.gets;
    let staged = match staged.and_then(|s| cancelled(cancel).map(|_| s)) {
        Ok(staged) => staged,
        Err(failure) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(failure);
        }
    };
    hooks::stop_at("after-stage");
    // 6. Rename onto the absent <dest>s.
    for (index, entry) in plan.entries.iter().enumerate() {
        fs::rename(
            staging.join(format!("tree.{index}")),
            state.join(&entry.dest),
        )
        .map_err(|e| io_failure(Code::StateInvalid, "rename staged tree", &e))?;
    }
    fsync_dir(state)?;
    fsync_dir(&staging)?;
    hooks::stop_at("after-rename");
    // 7. Commit.
    write_atomic(&fetch_dir, "receipt.json", &receipt_json(&plan, &staged))?;
    hooks::stop_at("after-commit");
    let _ = fs::remove_dir_all(&staging);
    let _ = fsync_dir(&fetch_dir);
    Ok(())
}
