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
use std::sync::Mutex;

pub const RECEIPT_SCHEMA: u64 = 1;
/// Free space kept for the rest of the system on top of every precheck.
pub const FREE_FLOOR: u64 = 32 << 20;

pub struct Config {
    pub run_id: String,
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
    /// This run holds `.pf-fetch/lock`, so it owns result.json.
    pub locked: bool,
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
    // The launcher probes this lock with a microsecond `try_lock_exclusive` (its watch polls at
    // 2 Hz); a run that lands inside that window must not lose to the probe. Retry briefly
    // (LOCK_TRIES attempts, LOCK_RETRY_DELAY apart, ~250 ms in all), then fail as before: a
    // genuine concurrent run holds the lock for its whole run. A loser still writes nothing
    // (W3 section 4.6): this function only returns the error.
    for attempt in 0..LOCK_TRIES {
        // SAFETY: flock(2) on a descriptor we own.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        if attempt + 1 < LOCK_TRIES {
            std::thread::sleep(LOCK_RETRY_DELAY);
        }
    }
    Err(state_invalid("another pf-app-fetch run holds the lock"))
}

/// Attempts to take the run lock before reporting another run holds it.
const LOCK_TRIES: u32 = 6;
/// Delay between lock attempts: 5 waits x 50 ms = 250 ms worst case.
const LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

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

/// Set once a run is inside the commit (steps 6-7). The main thread takes this lock before
/// acting on a cancel, so a cancel never races the rename or the receipt commit: it either
/// wins before step 6 (staging removed, `cancelled`) or waits for the commit to finish.
pub static COMMIT_GATE: Mutex<bool> = Mutex::new(false);

/// Everything `execute` needs, produced on the main thread by `prepare` (so the Landlock
/// domain covers the whole process before the worker thread exists).
pub struct Prepared {
    _lock: File,
    pub fetch_dir: PathBuf,
    state: PathBuf,
    plan: Plan,
    roots: Vec<ureq::tls::Certificate<'static>>,
    scope: Scope,
    overrides: BTreeMap<String, std::net::SocketAddr>,
}

/// The JSON written to `.pf-fetch/result.json` (atomically, 0644).
pub fn result_value(
    cfg: &Config,
    report: &Report,
    failure: Option<&Failure>,
    running: bool,
) -> Value {
    let (outcome, reason, class, retryable, detail) = match (running, failure) {
        (true, _) => (
            "running",
            Value::Null,
            Value::Null,
            Value::Null,
            String::new(),
        ),
        (false, None) => (
            "ready",
            Value::Null,
            Value::Null,
            Value::Null,
            String::new(),
        ),
        (false, Some(f)) => (
            "failed",
            json!(f.code.as_str()),
            json!(f.code.class()),
            json!(f.code.retryable()),
            truncate(&f.detail, 200),
        ),
    };
    json!({
        "schema_version": 1,
        "run_id": cfg.run_id,
        "app_id": cfg.app_id,
        "outcome": outcome,
        "reason": reason,
        "class": class,
        "retryable": retryable,
        "detail": detail,
        "check": report.check,
        "gets": report.gets,
    })
}

pub fn truncate(detail: &str, max: usize) -> String {
    let mut end = detail.len().min(max);
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    detail[..end].to_string()
}

pub fn write_result(fetch_dir: &Path, value: &Value) -> Result<()> {
    let mut text = serde_json::to_vec(value).expect("result serializes");
    text.push(b'\n');
    write_atomic(fetch_dir, "result.json", &text)
}

/// Main thread: take the lock FIRST (a run that loses it writes nothing, so it can never
/// clobber the live run's result.json), mark result.json `running` with this run's id, then
/// load the descriptor and CA bundle and apply Landlock.
pub fn prepare(cfg: &Config, report: &Mutex<Report>) -> Result<Prepared> {
    let state = cfg.state_dir.clone();
    let fetch_dir = prepare_fetch_dir(&state)?;
    let lock = lock(&fetch_dir)?;
    report.lock().unwrap_or_else(|e| e.into_inner()).locked = true;
    {
        let snapshot = report.lock().unwrap_or_else(|e| e.into_inner());
        write_result(&fetch_dir, &result_value(cfg, &snapshot, None, true))?;
    }
    let plan = descriptor::load(&cfg.descriptor, &cfg.app_id)?;
    let roots = http::load_roots(&cfg.ca_file)?;
    let table = hooks::redirect_table().unwrap_or_else(scope::platform_table);
    let first_hops: Vec<_> = plan.entries.iter().map(|e| e.url.clone()).collect();
    let scope = Scope::new(&first_hops, &table);
    let overrides = hooks::resolve_overrides();
    let mut ports = scope.ports();
    ports.extend(overrides.values().map(|addr| addr.port()));
    let landlock = confine::apply(cfg.confine, &state, &ports).map_err(state_invalid)?;
    report.lock().unwrap_or_else(|e| e.into_inner()).landlock = landlock;
    Ok(Prepared {
        _lock: lock,
        fetch_dir,
        state,
        plan,
        roots,
        scope,
        overrides,
    })
}

/// After a cancel that won the commit gate: remove this run's staging and moved-aside
/// trees (the same sweep a later run or the boot tmpfiles rule would do).
pub fn cleanup_after_cancel(fetch_dir: &Path) {
    let _ = sweep(fetch_dir);
}

/// Worker thread: steps 1-7.
pub fn execute(prep: Prepared, cancel: &AtomicBool, report: &Mutex<Report>) -> Result<()> {
    let Prepared {
        _lock,
        fetch_dir,
        state,
        plan,
        roots,
        scope,
        overrides,
    } = prep;
    let state = state.as_path();
    let set = |f: &dyn Fn(&mut Report)| f(&mut report.lock().unwrap_or_else(|e| e.into_inner()));

    // 1. Sweep.
    sweep(&fetch_dir)?;
    // 2. Check.
    let verdict = check(state, &fetch_dir, &plan);
    set(&|r| r.check = verdict);
    if verdict == "valid" {
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
    let gets = client.gets;
    set(&|r| r.gets = gets);
    let staged = match staged.and_then(|s| cancelled(cancel).map(|_| s)) {
        Ok(staged) => staged,
        Err(failure) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(failure);
        }
    };
    hooks::stop_at("after-stage");
    // Enter the commit only if no cancel has arrived; hold the gate through step 7.
    let mut gate = COMMIT_GATE.lock().unwrap_or_else(|e| e.into_inner());
    if cancel.load(Ordering::SeqCst) {
        drop(gate);
        let _ = fs::remove_dir_all(&staging);
        return Err(Failure::new(Code::Cancelled, "stop requested"));
    }
    *gate = true;
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
    drop(gate);
    let _ = fs::remove_dir_all(&staging);
    let _ = fsync_dir(&fetch_dir);
    Ok(())
}

#[cfg(test)]
mod lock_tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pf-fetch-lock-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn hold(dir: &Path) -> File {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("lock"))
            .unwrap();
        // SAFETY: flock(2) on a descriptor we own.
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        file
    }

    #[test]
    fn a_brief_foreign_hold_does_not_lose_the_run_its_lock() {
        // The launcher's `fetch_lock_held` probe takes the lock for microseconds; a run reaching
        // lock() inside that window must wait it out (tsp-ziac.4.3.10 N1), not fail state_invalid.
        let dir = scratch("brief");
        let probe = hold(&dir);
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            drop(probe);
        });
        let got = lock(&dir);
        releaser.join().unwrap();
        assert!(got.is_ok(), "{:?}", got.err().map(|e| e.detail));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_long_hold_still_loses_with_state_invalid_after_a_bounded_wait() {
        let dir = scratch("long");
        let _holder = hold(&dir);
        let started = Instant::now();
        let error = lock(&dir).expect_err("a real concurrent run keeps the lock");
        assert_eq!(error.code, Code::StateInvalid);
        assert!(error.detail.contains("holds the lock"), "{}", error.detail);
        assert!(started.elapsed() < Duration::from_secs(2));
        fs::remove_dir_all(dir).unwrap();
    }
}
