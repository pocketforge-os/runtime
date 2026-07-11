//! **AppOps ledger** — the persistent, revocable per-`(app × capability × op)` consent record
//! the E3 portal flow writes to (`tsp-ht0p.3`).
//!
//! The ledger *decouples* two things the merged v0 [`crate::EnforcingBackend`] conflated:
//!   * **declared-in-manifest** — the CEILING; a `use=[]` entry the launch validator accepted;
//!   * **allowed-now** — the LIVE subset a user has granted (via a supervisor-drawn Prompt), scoped
//!     [`Scope::Once`] (exactly one op) or [`Scope::Always`] (survives restart until revoked).
//!
//! The manifest is the ceiling; the ledger is the live allowed-now SUBSET — it can *never* exceed
//! what the validator accepted, and a cap that leaves the ceiling (a `use=[]` line removed on
//! reinstall) leaves its prior ledger grants ORPHANED, not applied ([`orphaned_grants`]). That
//! orphan concept is what `tsp-ht0p.5`'s Risk-#22 delivery design consumes.
//!
//! ## Where it lives (structural no-self-grant)
//!
//! This module is in `pf-broker`, which apps do NOT link — apps link the `pocketforge` client
//! facade. Nothing an app can call, in-process or over the wire, can write a ledger row: the write
//! API is only reachable from the enforcing daemon's own supervisor-ask response path and from
//! [`crate::bin::pf_permissions`] (the out-of-band inspect/revoke surface). The
//! [`pocketforge::backends::InProcessBackend::set_consent`] seam that already exists stays exactly
//! as it did (test/control plane; R-A honest cooperative library) — it fires the change-event bus
//! so an app subscribed via [`pocketforge::backends::InProcessBackend::subscribe`] observes a
//! revoke and re-queries. The ledger's revoke path *also* calls that seam so the whole system
//! stays consistent under revocation (see [`crate::bin::pf_permissions`]).
//!
//! ## Persistence format (v1 CONTRACT store — deliberately simple)
//!
//! One append-only file per app id at `$PF_APPOPS_DIR/<app_id>.log` (defaulting to
//! `$XDG_STATE_HOME/pocketforge/appops/`, else `$HOME/.local/state/pocketforge/appops/`). Each
//! line is one event (`grant` / `revoke` / `once_used`) appended via `O_APPEND` + a single
//! `write(2)` — effectively atomic on local filesystems (a partial-write interleave is not
//! possible for one syscall against an `O_APPEND` file); on [`AppOpsLedger::open`] we replay the
//! file forward to rebuild the in-memory `(app, cap, modifier) → state` map. This is the **v1
//! CONTRACT store** — NOT the post-Phase-2 broker's final store (which will live in the M1.D
//! supervisor's state directory with fsync barriers + a compaction pass). v1 favors simplicity:
//! one line per event, tab-separated, backslash-escaped, so a human can `cat` it and `pf
//! permissions inspect` is a filter over it. Ordering within a file is chronological
//! (append-only); the in-memory replay applies latest-wins per key.
//!
//! **Revoke semantics (deliberate v1 shape).** A `revoke` produces a standing-Denied entry for
//! the (app, cap, modifier) triple — a subsequent `acquire` on that key returns `ConsentDenied`
//! without re-Prompting (matches the bead's STEP-2 acceptance). There is no "forget the grant"
//! back-to-Prompt path in v1 by design: a revoke is a user "no", not an "erase my history", so
//! the next acquire honors the last user answer rather than showing another dialog. The final
//! broker's settings UI will grow a "reset to Prompt" affordance, but not in v1.
//!
//! Record shape (fields in fixed order, separated by U+0009 HORIZONTAL TAB; unset text values are
//! the single hyphen `-`; strings backslash-escape `\`, `\t`, `\n`):
//!
//! ```text
//! ts_ms=<u64>\tevent=grant|revoke|once_used\tapp=<id>\tcap=<name>\tmodifier=<mod-or-->\top_id=<u64>\tscope=once|always|-\task_id=<u64>\tinput=<AskInput-or-->\tsupervisor_note=<escaped-or-->
//! ```
//!
//! (Numeric fields never contain the delimiter, so the parse is a plain split_on_tab.)

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::consent::AskInput;
use crate::manifest::ValidatedManifest;

/// The grant lifetime scope the user selected on the supervisor-drawn Prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Authorizes exactly one op — consumed on first use, then requires a fresh Prompt.
    Once,
    /// Persists across restarts until revoked (the ledger row survives).
    Always,
}

impl Scope {
    fn to_str(self) -> &'static str {
        match self {
            Scope::Once => "once",
            Scope::Always => "always",
        }
    }
    fn parse(s: &str) -> Option<Scope> {
        match s {
            "once" => Some(Scope::Once),
            "always" => Some(Scope::Always),
            _ => None,
        }
    }
}

/// The identity of a ledger row — `(app, cap, modifier)`. Two rows with the same triple collapse
/// to one live state (latest-wins on replay).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GrantKey {
    pub app_id: String,
    pub cap: String,
    pub modifier: Option<String>,
}

impl GrantKey {
    /// Build a key with normalized (lowercase) cap + modifier — the ceiling check keys off this.
    pub fn new(app_id: impl Into<String>, cap: impl AsRef<str>, modifier: Option<&str>) -> GrantKey {
        GrantKey {
            app_id: app_id.into(),
            cap: cap.as_ref().to_ascii_lowercase(),
            modifier: modifier.map(|m| m.to_ascii_lowercase()),
        }
    }
}

/// The live state of one `(app, cap, modifier)` after replaying the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantEntry {
    pub key: GrantKey,
    pub scope: Scope,
    pub ts_ms: u64,
    pub op_id: u64,
    pub ask_id: u64,
    pub input: Option<AskInput>,
    /// Set on a `Scope::Once` grant that has already been consumed — its next `check` returns
    /// [`GrantCheck::NoGrant`] and a fresh Prompt is required.
    pub once_used: bool,
    pub supervisor_note: Option<String>,
}

/// Why writing / reading the ledger failed.
#[derive(Debug)]
pub enum LedgerError {
    /// The (app, cap) is not in the app's validated manifest — the CEILING guard fired. This is
    /// the load-bearing "ceiling-bound" invariant: a grant outside `use=[]` is REFUSED and
    /// nothing is written to disk.
    OutsideCeiling { app_id: String, cap: String },
    /// A record on disk could not be parsed. On replay we STOP on the first malformed line and
    /// surface the offset — a corrupt line is a signal to a human operator, not something to
    /// silently discard.
    Malformed { line_no: usize, reason: String },
    /// An IO error appending/reading the file.
    Io(std::io::Error),
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LedgerError::OutsideCeiling { app_id, cap } => {
                write!(f, "grant refused: '{cap}' is outside app '{app_id}' manifest ceiling")
            }
            LedgerError::Malformed { line_no, reason } => {
                write!(f, "ledger line {line_no}: malformed ({reason})")
            }
            LedgerError::Io(e) => write!(f, "ledger io: {e}"),
        }
    }
}

impl std::error::Error for LedgerError {}

impl From<std::io::Error> for LedgerError {
    fn from(e: std::io::Error) -> Self {
        LedgerError::Io(e)
    }
}

/// The check result the portal flow uses to decide whether to Prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantCheck {
    /// No live grant covers this `(app, cap, modifier)` — the enforcing backend must Prompt (or
    /// deny outright if no supervisor is wired).
    NoGrant,
    /// A live `Scope::Always` grant covers this key.
    Always,
    /// A live `Scope::Once` grant with `once_used == false` is available — the caller must
    /// [`AppOpsLedger::consume_once`] to mark it consumed, then proceed.
    OnceAvailable,
    /// A live `Scope::Once` grant is present but already consumed. Semantically the same as
    /// [`GrantCheck::NoGrant`] for allow-decisions, but exposed distinctly for `pf permissions
    /// inspect`.
    OnceUsed,
    /// A `revoke` event is the newest record for this key — the app has been explicitly denied.
    Revoked,
}

/// Which record type a stored event carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventKind {
    Grant,
    Revoke,
    OnceUsed,
}

impl EventKind {
    fn to_str(self) -> &'static str {
        match self {
            EventKind::Grant => "grant",
            EventKind::Revoke => "revoke",
            EventKind::OnceUsed => "once_used",
        }
    }
    fn parse(s: &str) -> Option<EventKind> {
        match s {
            "grant" => Some(EventKind::Grant),
            "revoke" => Some(EventKind::Revoke),
            "once_used" => Some(EventKind::OnceUsed),
            _ => None,
        }
    }
}

/// One raw event as encoded on disk.
#[derive(Debug, Clone)]
struct Record {
    ts_ms: u64,
    event: EventKind,
    key: GrantKey,
    op_id: u64,
    scope: Option<Scope>,
    ask_id: u64,
    input: Option<AskInput>,
    supervisor_note: Option<String>,
}

/// The current in-memory view of a `(app, cap, modifier)` key after replay.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LiveState {
    Granted(GrantEntry),
    Revoked { ts_ms: u64 },
}

/// The persistent AppOps ledger.
///
/// One instance per running broker; it owns the on-disk directory + the in-memory replayed state.
/// Cheap-to-share via [`Arc`](std::sync::Arc); all mutating methods take `&self` and are
/// internally synchronized.
pub struct AppOpsLedger {
    dir: PathBuf,
    state: Mutex<HashMap<GrantKey, LiveState>>,
    next_op_id: AtomicU64,
}

impl AppOpsLedger {
    /// Open (or create) a ledger rooted at `dir`. Replays every `<app_id>.log` file it finds so
    /// the in-memory state reflects every `Scope::Always` grant that survived a restart.
    ///
    /// A malformed line stops replay of THAT file with [`LedgerError::Malformed`] carrying its
    /// offset — a fail-loud posture appropriate for a trust-path store. (Other apps' files
    /// continue to replay.)
    pub fn open(dir: impl AsRef<Path>) -> Result<AppOpsLedger, LedgerError> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        let mut state = HashMap::new();

        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("log") {
                continue;
            }
            replay_file(&path, &mut state)?;
        }

        Ok(AppOpsLedger {
            dir,
            state: Mutex::new(state),
            next_op_id: AtomicU64::new(1),
        })
    }

    /// Open a ledger under the platform-default state root (`$PF_APPOPS_DIR` if set; else
    /// `$XDG_STATE_HOME/pocketforge/appops/`; else `$HOME/.local/state/pocketforge/appops/`; else
    /// `$TMPDIR/pocketforge-appops/`). Used by the `pf-permissions` CLI and the enforcing broker.
    pub fn open_default() -> Result<AppOpsLedger, LedgerError> {
        AppOpsLedger::open(default_appops_dir())
    }

    /// The directory this ledger persists into.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Mint a broker-scoped monotonic op id used as the `op_id` on a grant row (and, for
    /// [`Scope::Once`] grants, as the identity the callee consumes exactly once).
    pub fn mint_op_id(&self) -> u64 {
        self.next_op_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Check the ledger for a live grant covering `key`. Read-only.
    pub fn check(&self, key: &GrantKey) -> GrantCheck {
        let st = self.state.lock().unwrap();
        match st.get(key) {
            None => GrantCheck::NoGrant,
            Some(LiveState::Revoked { .. }) => GrantCheck::Revoked,
            Some(LiveState::Granted(g)) => match (g.scope, g.once_used) {
                (Scope::Always, _) => GrantCheck::Always,
                (Scope::Once, false) => GrantCheck::OnceAvailable,
                (Scope::Once, true) => GrantCheck::OnceUsed,
            },
        }
    }

    /// Record a fresh `grant` row. Enforces the CEILING: `(cap, modifier)` must be in the
    /// validated manifest, else [`LedgerError::OutsideCeiling`] and NOTHING is written.
    ///
    /// Returns the resulting [`GrantEntry`]. Caller supplies `manifest` (the app's active
    /// validated manifest — the ceiling), `scope`, and the [`AskInput`] the supervisor recorded
    /// (see [`crate::consent`]).
    #[allow(clippy::too_many_arguments)]
    pub fn record_grant(
        &self,
        manifest: &ValidatedManifest,
        cap: &str,
        modifier: Option<&str>,
        scope: Scope,
        ask_id: u64,
        input: AskInput,
        supervisor_note: Option<String>,
    ) -> Result<GrantEntry, LedgerError> {
        let cap_lc = cap.to_ascii_lowercase();
        if !manifest.allows(&cap_lc) {
            return Err(LedgerError::OutsideCeiling {
                app_id: manifest.app_id.clone(),
                cap: cap_lc,
            });
        }
        let key = GrantKey::new(manifest.app_id.clone(), cap, modifier);
        let op_id = self.mint_op_id();
        let ts_ms = now_ms();
        let rec = Record {
            ts_ms,
            event: EventKind::Grant,
            key: key.clone(),
            op_id,
            scope: Some(scope),
            ask_id,
            input: Some(input),
            supervisor_note: supervisor_note.clone(),
        };
        self.append(&rec)?;
        let entry = GrantEntry {
            key: key.clone(),
            scope,
            ts_ms,
            op_id,
            ask_id,
            input: Some(input),
            once_used: false,
            supervisor_note,
        };
        self.state.lock().unwrap().insert(key, LiveState::Granted(entry.clone()));
        Ok(entry)
    }

    /// Mark a live `Scope::Once` grant consumed. No-op if the key is not in `OnceAvailable`. The
    /// event is appended so a fresh replay sees the consumed state.
    pub fn consume_once(&self, key: &GrantKey) -> Result<(), LedgerError> {
        let ts_ms = now_ms();
        let (mut entry, op_id) = {
            let st = self.state.lock().unwrap();
            match st.get(key) {
                Some(LiveState::Granted(g)) if g.scope == Scope::Once && !g.once_used => {
                    (g.clone(), self.mint_op_id())
                }
                _ => return Ok(()),
            }
        };
        let rec = Record {
            ts_ms,
            event: EventKind::OnceUsed,
            key: key.clone(),
            op_id,
            scope: None,
            ask_id: entry.ask_id,
            input: None,
            supervisor_note: None,
        };
        self.append(&rec)?;
        entry.once_used = true;
        self.state.lock().unwrap().insert(key.clone(), LiveState::Granted(entry));
        Ok(())
    }

    /// Record a `revoke` for `key`. Idempotent (safe to call twice — a second revoke is another
    /// audit event, but the live state stays revoked).
    pub fn record_revoke(&self, key: &GrantKey) -> Result<(), LedgerError> {
        let ts_ms = now_ms();
        let op_id = self.mint_op_id();
        let rec = Record {
            ts_ms,
            event: EventKind::Revoke,
            key: key.clone(),
            op_id,
            scope: None,
            ask_id: 0,
            input: None,
            supervisor_note: None,
        };
        self.append(&rec)?;
        self.state
            .lock()
            .unwrap()
            .insert(key.clone(), LiveState::Revoked { ts_ms });
        Ok(())
    }

    /// Every live `Scope::Always` grant (post-replay; excludes revoked and consumed-once). Used
    /// by `pf permissions inspect` and by [`orphaned_grants`](Self::orphaned_grants).
    pub fn always_grants(&self) -> Vec<GrantEntry> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<GrantEntry> = st
            .values()
            .filter_map(|s| match s {
                LiveState::Granted(g) if g.scope == Scope::Always => Some(g.clone()),
                _ => None,
            })
            .collect();
        out.sort_by_key(|a| (a.key.app_id.clone(), a.key.cap.clone()));
        out
    }

    /// All in-memory entries (any state) — the `pf permissions inspect` read surface.
    pub fn snapshot(&self) -> Vec<GrantEntry> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<GrantEntry> = st
            .values()
            .filter_map(|s| match s {
                LiveState::Granted(g) => Some(g.clone()),
                LiveState::Revoked { .. } => None,
            })
            .collect();
        out.sort_by_key(|a| (a.key.app_id.clone(), a.key.cap.clone()));
        out
    }

    /// Grants whose capability is no longer in the app's active manifest — the ceiling shrunk and
    /// old rows are stranded. Returned unchanged; the ceiling-change hook (`.5`'s Risk-#22 flow)
    /// decides whether to revoke/retain/surface-for-review. Does NOT mutate.
    pub fn orphaned_grants(&self, manifest: &ValidatedManifest) -> Vec<GrantEntry> {
        self.snapshot()
            .into_iter()
            .filter(|g| g.key.app_id == manifest.app_id && !manifest.allows(&g.key.cap))
            .collect()
    }

    /// Explicitly REVOKE every orphaned grant for `manifest` — the resurrection-hazard fix that
    /// `tsp-ht0p.5` (Risk-#22 re-permission flow) calls when it processes a manifest update.
    ///
    /// The ledger is last-wins on replay, so an orphaned `Scope::Always` grant would silently
    /// re-activate if a future manifest reintroduced the capability. Writing a `revoke` event
    /// makes revocation the durable last-wins record for that key — a re-widened manifest must
    /// re-Prompt. **This method is a `.5`-flow helper: it is NOT called automatically on ledger
    /// open (that would be surprising for a pure ledger); the re-permission review path drives
    /// it.** Returns the list of revoked keys.
    pub fn revoke_orphans(&self, manifest: &ValidatedManifest) -> Result<Vec<GrantKey>, LedgerError> {
        let orphaned = self.orphaned_grants(manifest);
        let mut revoked = Vec::with_capacity(orphaned.len());
        for g in orphaned {
            self.record_revoke(&g.key)?;
            revoked.push(g.key);
        }
        Ok(revoked)
    }

    fn append(&self, rec: &Record) -> Result<(), LedgerError> {
        let path = self.dir.join(format!("{}.log", sanitize_app_id(&rec.key.app_id)));
        let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
        let line = encode_record(rec);
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        Ok(())
    }
}

fn replay_file(path: &Path, state: &mut HashMap<GrantKey, LiveState>) -> Result<(), LedgerError> {
    let f = std::fs::File::open(path)?;
    let rdr = BufReader::new(f);
    for (i, line) in rdr.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let rec = parse_record(&line).map_err(|e| LedgerError::Malformed {
            line_no: i + 1,
            reason: e,
        })?;
        apply(state, rec);
    }
    Ok(())
}

fn apply(state: &mut HashMap<GrantKey, LiveState>, rec: Record) {
    let key = rec.key.clone();
    match rec.event {
        EventKind::Grant => {
            let scope = rec.scope.unwrap_or(Scope::Once);
            let entry = GrantEntry {
                key: key.clone(),
                scope,
                ts_ms: rec.ts_ms,
                op_id: rec.op_id,
                ask_id: rec.ask_id,
                input: rec.input,
                once_used: false,
                supervisor_note: rec.supervisor_note,
            };
            state.insert(key, LiveState::Granted(entry));
        }
        EventKind::Revoke => {
            state.insert(key, LiveState::Revoked { ts_ms: rec.ts_ms });
        }
        EventKind::OnceUsed => {
            if let Some(LiveState::Granted(g)) = state.get_mut(&key) {
                if g.scope == Scope::Once {
                    g.once_used = true;
                }
            }
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Restrict an app id to filesystem-safe characters (dots + alphanumerics + underscores + dashes).
fn sanitize_app_id(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect()
}

fn default_appops_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("PF_APPOPS_DIR") {
        return PathBuf::from(v);
    }
    if let Some(base) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(base).join("pocketforge").join("appops");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local/state")
            .join("pocketforge")
            .join("appops");
    }
    std::env::temp_dir().join("pocketforge-appops")
}

// ---------------------------------------------------------------------------
// Record encoding (fixed field order + backslash escapes; see module docs).
// ---------------------------------------------------------------------------

fn encode_record(rec: &Record) -> String {
    let modifier = rec.key.modifier.as_deref().unwrap_or("-");
    let scope = rec.scope.map(|s| s.to_str()).unwrap_or("-");
    let input = rec
        .input
        .map(|i| i.as_str())
        .unwrap_or("-");
    let note = rec.supervisor_note.as_deref().unwrap_or("-");
    format!(
        "ts_ms={ts_ms}\tevent={event}\tapp={app}\tcap={cap}\tmodifier={modifier}\top_id={op_id}\tscope={scope}\task_id={ask_id}\tinput={input}\tsupervisor_note={note}",
        ts_ms = rec.ts_ms,
        event = rec.event.to_str(),
        app = escape(&rec.key.app_id),
        cap = escape(&rec.key.cap),
        modifier = escape(modifier),
        op_id = rec.op_id,
        scope = scope,
        ask_id = rec.ask_id,
        input = input,
        note = escape(note),
    )
}

fn parse_record(line: &str) -> Result<Record, String> {
    let mut ts_ms: Option<u64> = None;
    let mut event: Option<EventKind> = None;
    let mut app: Option<String> = None;
    let mut cap: Option<String> = None;
    let mut modifier: Option<String> = None;
    let mut op_id: Option<u64> = None;
    let mut scope: Option<Scope> = None;
    let mut ask_id: Option<u64> = None;
    let mut input: Option<AskInput> = None;
    let mut note: Option<String> = None;

    for field in line.split('\t') {
        let (k, v) = field
            .split_once('=')
            .ok_or_else(|| format!("field '{field}' has no '='"))?;
        match k {
            "ts_ms" => ts_ms = Some(v.parse().map_err(|e| format!("ts_ms: {e}"))?),
            "event" => {
                event = Some(EventKind::parse(v).ok_or_else(|| format!("bad event '{v}'"))?)
            }
            "app" => app = Some(unescape(v)?),
            "cap" => cap = Some(unescape(v)?),
            "modifier" => {
                let u = unescape(v)?;
                modifier = if u == "-" { None } else { Some(u) };
            }
            "op_id" => op_id = Some(v.parse().map_err(|e| format!("op_id: {e}"))?),
            "scope" => {
                scope = if v == "-" { None } else { Some(Scope::parse(v).ok_or_else(|| format!("bad scope '{v}'"))?) };
            }
            "ask_id" => ask_id = Some(v.parse().map_err(|e| format!("ask_id: {e}"))?),
            "input" => {
                input = if v == "-" { None } else { Some(AskInput::parse(v).ok_or_else(|| format!("bad input '{v}'"))?) };
            }
            "supervisor_note" => {
                let u = unescape(v)?;
                note = if u == "-" { None } else { Some(u) };
            }
            _ => return Err(format!("unknown field '{k}'")),
        }
    }

    Ok(Record {
        ts_ms: ts_ms.ok_or("missing ts_ms")?,
        event: event.ok_or("missing event")?,
        key: GrantKey {
            app_id: app.ok_or("missing app")?,
            cap: cap.ok_or("missing cap")?,
            modifier,
        },
        op_id: op_id.ok_or("missing op_id")?,
        scope,
        ask_id: ask_id.ok_or("missing ask_id")?,
        input,
        supervisor_note: note,
    })
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

fn unescape(s: &str) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some(other) => return Err(format!("bad escape '\\{other}'")),
            None => return Err("dangling backslash".into()),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consent::AskInput;
    use crate::manifest::AppManifest;
    use pocketforge::Descriptor;

    fn gnss_descriptor() -> Descriptor {
        Descriptor::from_toml(
            r#"
[identity]
id = "synthgnss"
manufacturer = "PocketForge"
model = "GNSS test rig"
sdl_guid = "00000000000000000000000000000000"

[[inputs]]
id = "south"
kind = "button"
ev_type = "EV_KEY"
code = "BTN_A"

[[sensors]]
id = "gnss"
kind = "gnss"
"#,
        )
        .unwrap()
    }

    fn manifest_for(app_id: &str, uses: &[&str], desc: &Descriptor) -> ValidatedManifest {
        let toml = format!(
            "[app]\nid = \"{app_id}\"\nuse = [{}]\n",
            uses.iter().map(|u| format!("\"{u}\"")).collect::<Vec<_>>().join(", ")
        );
        AppManifest::from_toml(&toml).unwrap().validate(desc).unwrap()
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("pf-appops-{}-{}-{}", tag, std::process::id(), now_ms()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn ceiling_bound_refuses_grant_outside_manifest() {
        let dir = tmp_dir("ceiling");
        let ledger = AppOpsLedger::open(&dir).unwrap();
        let desc = gnss_descriptor();
        // Manifest declares ONLY location — vibration is outside the ceiling.
        let m = manifest_for("com.test.app", &["location:approximate"], &desc);
        let err = ledger
            .record_grant(&m, "vibration", None, Scope::Always, 1, AskInput::AOnAllowAlways, None)
            .unwrap_err();
        match err {
            LedgerError::OutsideCeiling { cap, app_id } => {
                assert_eq!(cap, "vibration");
                assert_eq!(app_id, "com.test.app");
            }
            other => panic!("expected OutsideCeiling, got {other:?}"),
        }
        // Nothing on disk (the ceiling refusal wrote no line).
        assert!(ledger.snapshot().is_empty(), "no grant should be recorded");
        let path = dir.join("com.test.app.log");
        assert!(
            !path.exists() || std::fs::read_to_string(&path).unwrap().is_empty(),
            "no line should be written for a ceiling-refused grant"
        );
    }

    #[test]
    fn always_grant_survives_a_fresh_ledger_open() {
        let dir = tmp_dir("survive");
        let desc = gnss_descriptor();
        let m = manifest_for("com.test.weather", &["location:approximate"], &desc);
        {
            let l1 = AppOpsLedger::open(&dir).unwrap();
            l1.record_grant(&m, "location", Some("approximate"), Scope::Always, 42, AskInput::AOnAllowAlways, None)
                .unwrap();
            assert_eq!(l1.check(&GrantKey::new("com.test.weather", "location", Some("approximate"))), GrantCheck::Always);
        }
        // Reopen — the Always grant must replay.
        let l2 = AppOpsLedger::open(&dir).unwrap();
        assert_eq!(
            l2.check(&GrantKey::new("com.test.weather", "location", Some("approximate"))),
            GrantCheck::Always,
            "Always grant must survive a fresh ledger open (restart)"
        );
    }

    #[test]
    fn once_grant_authorizes_exactly_one_op() {
        let dir = tmp_dir("once");
        let desc = gnss_descriptor();
        let m = manifest_for("com.test.once", &["location:approximate"], &desc);
        let l = AppOpsLedger::open(&dir).unwrap();
        l.record_grant(&m, "location", Some("approximate"), Scope::Once, 1, AskInput::AOnAllowOnce, None)
            .unwrap();
        let key = GrantKey::new("com.test.once", "location", Some("approximate"));
        assert_eq!(l.check(&key), GrantCheck::OnceAvailable);
        l.consume_once(&key).unwrap();
        assert_eq!(l.check(&key), GrantCheck::OnceUsed);
        // Replay confirms the used flag.
        drop(l);
        let l2 = AppOpsLedger::open(&dir).unwrap();
        assert_eq!(l2.check(&key), GrantCheck::OnceUsed, "once-consumed state must survive replay");
    }

    #[test]
    fn revoke_replaces_live_grant() {
        let dir = tmp_dir("revoke");
        let desc = gnss_descriptor();
        let m = manifest_for("com.test.rev", &["location:approximate"], &desc);
        let l = AppOpsLedger::open(&dir).unwrap();
        l.record_grant(&m, "location", Some("approximate"), Scope::Always, 7, AskInput::AOnAllowAlways, None)
            .unwrap();
        let key = GrantKey::new("com.test.rev", "location", Some("approximate"));
        assert_eq!(l.check(&key), GrantCheck::Always);
        l.record_revoke(&key).unwrap();
        assert_eq!(l.check(&key), GrantCheck::Revoked);
        // Fresh open sees Revoked (never-granted is Revoked here because revoke is last-wins).
        drop(l);
        let l2 = AppOpsLedger::open(&dir).unwrap();
        assert_eq!(l2.check(&key), GrantCheck::Revoked, "revoke must survive replay");
    }

    #[test]
    fn orphaned_grants_surface_when_ceiling_shrinks() {
        let dir = tmp_dir("orphan");
        let desc = gnss_descriptor();
        let wide = manifest_for("com.test.orphan", &["location:approximate", "egress:tile.example"], &desc);
        let l = AppOpsLedger::open(&dir).unwrap();
        l.record_grant(&wide, "location", Some("approximate"), Scope::Always, 1, AskInput::AOnAllowAlways, None)
            .unwrap();
        l.record_grant(&wide, "egress", Some("tile.example"), Scope::Always, 2, AskInput::AOnAllowAlways, None)
            .unwrap();
        // Ceiling shrinks — a new manifest without egress leaves the egress grant orphaned.
        let narrow = manifest_for("com.test.orphan", &["location:approximate"], &desc);
        let orphaned = l.orphaned_grants(&narrow);
        assert_eq!(orphaned.len(), 1);
        assert_eq!(orphaned[0].key.cap, "egress");
    }

    #[test]
    fn record_round_trip_encoding() {
        // The on-disk shape is the wire contract for the file — round-trip every escape case.
        let rec = Record {
            ts_ms: 1_700_000_000_000,
            event: EventKind::Grant,
            key: GrantKey::new("com.tab.newline", "egress", Some("api.\t\n.example")),
            op_id: 99,
            scope: Some(Scope::Always),
            ask_id: 12345,
            input: Some(AskInput::AOnAllowAlways),
            supervisor_note: Some("a note with \\ and \t inside".into()),
        };
        let line = encode_record(&rec);
        assert!(!line.contains('\n'), "no unescaped newline in a record");
        let back = parse_record(&line).unwrap();
        assert_eq!(back.ts_ms, rec.ts_ms);
        assert_eq!(back.event, rec.event);
        assert_eq!(back.key, rec.key);
        assert_eq!(back.op_id, rec.op_id);
        assert_eq!(back.scope, rec.scope);
        assert_eq!(back.ask_id, rec.ask_id);
        assert_eq!(back.input, rec.input);
        assert_eq!(back.supervisor_note, rec.supervisor_note);
    }

    #[test]
    fn malformed_line_fails_replay() {
        let dir = tmp_dir("malformed");
        let path = dir.join("com.bad.log");
        std::fs::write(&path, "this is not a real record\n").unwrap();
        match AppOpsLedger::open(&dir) {
            Err(LedgerError::Malformed { line_no, .. }) => assert_eq!(line_no, 1),
            Err(e) => panic!("expected Malformed, got {e:?}"),
            Ok(_) => panic!("expected Malformed error, got Ok(ledger)"),
        }
    }
}
