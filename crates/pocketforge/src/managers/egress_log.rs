//! The **egress log** (`tsp-ht0p.4`) — the per-`(app × host)` audit trail the
//! cooperative-accounting egress manager appends to for every declared-host `send` and every
//! undeclared-host REFUSAL. Persists in the same tab-separated / backslash-escaped JSONL-ish
//! shape as [`pf_broker::appops`]'s AppOps ledger so the `pf-permissions` CLI reads both
//! stores through one parse dialect — this is what the bead work order calls "events join
//! .3's AppOps JSONL store format" (same shape, sibling directory; the consent ledger and the
//! egress log are DELIBERATELY separate files so a consumer that greps one never confuses a
//! grant row with a byte-accounting row).
//!
//! ## Honesty (R-A)
//!
//! Every event here is COOPERATIVE ACCOUNTING (Q1 ruling, 2026-07-11: v1 = declaration +
//! ledger + accounting; no netns/nftables teeth). A `send` event proves the app declared the
//! host and consumed a byte-quota tick; a `refused` event proves the manager REFUSED to
//! account an undeclared host (returning [`crate::CapError::PolicyBlocked`] to the caller) —
//! the app could still bypass the manager and hit the kernel directly in v1. The enforcement
//! seam that closes that gap is designed in `docs/EGRESS-ENFORCEMENT-SEAM.md` and tracked by
//! the follow-on bead filed at merge.
//!
//! ## Persistence format
//!
//! One append-only `<app>.log` per app at `$PF_EGRESS_LOG_DIR/` (default:
//! `$XDG_STATE_HOME/pocketforge/egress/`, else `$HOME/.local/state/pocketforge/egress/`, else
//! `$TMPDIR/pocketforge-egress/`). Each line is a single event, tab-separated, with the same
//! `\`/`\t`/`\n` escapes and `-` = unset sentinel as the AppOps ledger:
//!
//! ```text
//! ts_ms=<u64>\tevent=send|refused\tapp=<id>\thost=<host>\tbytes=<u64>\tremaining_ops=<u64>\treason=<escaped-or-->
//! ```
//!
//! Ordering within a file is chronological (append-only). Malformed lines STOP the replay of
//! that file and surface the offset — matches the AppOps fail-loud posture (a corrupt trust
//! log is not silently discarded).

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Which kind of egress event was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressEventKind {
    /// A declared-host send accounted against the byte tally + op bucket.
    Send,
    /// An undeclared-host send REFUSED by the cooperative accountant — the manager returned
    /// [`crate::CapError::PolicyBlocked`] without touching any bucket.
    Refused,
}

impl EgressEventKind {
    fn to_str(self) -> &'static str {
        match self {
            EgressEventKind::Send => "send",
            EgressEventKind::Refused => "refused",
        }
    }
    fn parse(s: &str) -> Option<EgressEventKind> {
        match s {
            "send" => Some(EgressEventKind::Send),
            "refused" => Some(EgressEventKind::Refused),
            _ => None,
        }
    }
}

/// One recorded egress event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressEvent {
    /// Milliseconds since the Unix epoch when the event was appended.
    pub ts_ms: u64,
    /// Kind of event (`send` or `refused`).
    pub event: EgressEventKind,
    /// Application id (from the validated manifest).
    pub app_id: String,
    /// The declared or attempted destination host.
    pub host: String,
    /// Bytes reported by the caller (zero for a `refused` event).
    pub bytes: u64,
    /// `egress` op-bucket tokens remaining after the event.
    pub remaining_ops: u64,
    /// Human-readable reason for a `refused` event; `None` for a `send`.
    pub reason: Option<String>,
}

/// Why writing / reading the egress log failed.
#[derive(Debug)]
pub enum EgressLogError {
    /// A record on disk could not be parsed. Same fail-loud posture as
    /// [`pf_broker::appops::LedgerError::Malformed`].
    Malformed { line_no: usize, reason: String },
    /// An IO error appending/reading the file.
    Io(std::io::Error),
}

impl std::fmt::Display for EgressLogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EgressLogError::Malformed { line_no, reason } => {
                write!(f, "egress log line {line_no}: malformed ({reason})")
            }
            EgressLogError::Io(e) => write!(f, "egress log io: {e}"),
        }
    }
}

impl std::error::Error for EgressLogError {}

impl From<std::io::Error> for EgressLogError {
    fn from(e: std::io::Error) -> Self {
        EgressLogError::Io(e)
    }
}

/// The persistent egress event log — one append-only file per app id.
pub struct EgressLog {
    dir: PathBuf,
    // Guards concurrent appends: two managers sharing an Arc<EgressLog> must not interleave
    // partial writes at the syscall level (append-only-on-linux is atomic per single write, but
    // callers here may still stack multiple lines under one call in the future).
    write_guard: Mutex<()>,
}

impl EgressLog {
    /// Open (or create) an egress log rooted at `dir`. Creates the directory if absent.
    pub fn open(dir: impl AsRef<Path>) -> Result<EgressLog, EgressLogError> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        Ok(EgressLog { dir, write_guard: Mutex::new(()) })
    }

    /// Open the log at the platform-default state root.
    pub fn open_default() -> Result<EgressLog, EgressLogError> {
        EgressLog::open(default_egress_log_dir())
    }

    /// The directory this log persists into.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Append a `send` event for `(app, host, bytes)` with the ops-remaining after the send.
    pub fn record_send(
        &self,
        app_id: &str,
        host: &str,
        bytes: u64,
        remaining_ops: u64,
    ) -> Result<EgressEvent, EgressLogError> {
        let event = EgressEvent {
            ts_ms: now_ms(),
            event: EgressEventKind::Send,
            app_id: app_id.to_string(),
            host: host.to_string(),
            bytes,
            remaining_ops,
            reason: None,
        };
        self.append(&event)?;
        Ok(event)
    }

    /// Append a `refused` event with a human-readable `reason` (typically
    /// `"host <h> not declared"` or `"op quota exhausted"`).
    pub fn record_refused(
        &self,
        app_id: &str,
        host: &str,
        reason: impl Into<String>,
        remaining_ops: u64,
    ) -> Result<EgressEvent, EgressLogError> {
        let event = EgressEvent {
            ts_ms: now_ms(),
            event: EgressEventKind::Refused,
            app_id: app_id.to_string(),
            host: host.to_string(),
            bytes: 0,
            remaining_ops,
            reason: Some(reason.into()),
        };
        self.append(&event)?;
        Ok(event)
    }

    /// Replay every `<app>.log` file in the directory and return every event chronologically
    /// (per-file — cross-file ordering is app id lexicographic then per-file chronological).
    /// The inspection surface calls this + filters by app.
    pub fn snapshot(&self) -> Result<Vec<EgressEvent>, EgressLogError> {
        let mut apps: Vec<PathBuf> = std::fs::read_dir(&self.dir)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("log"))
            .collect();
        apps.sort();
        let mut out = Vec::new();
        for path in apps {
            replay_file(&path, &mut out)?;
        }
        Ok(out)
    }

    /// Replay + filter to a single app id.
    pub fn snapshot_for(&self, app_id: &str) -> Result<Vec<EgressEvent>, EgressLogError> {
        let path = self.dir.join(format!("{}.log", sanitize_app_id(app_id)));
        let mut out = Vec::new();
        if path.exists() {
            replay_file(&path, &mut out)?;
        }
        Ok(out)
    }

    fn append(&self, event: &EgressEvent) -> Result<(), EgressLogError> {
        let _lock = self.write_guard.lock().unwrap();
        let path = self.dir.join(format!("{}.log", sanitize_app_id(&event.app_id)));
        let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
        let line = encode(event);
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        Ok(())
    }
}

fn replay_file(path: &Path, out: &mut Vec<EgressEvent>) -> Result<(), EgressLogError> {
    let f = std::fs::File::open(path)?;
    let rdr = BufReader::new(f);
    for (i, line) in rdr.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let event = parse(&line).map_err(|e| EgressLogError::Malformed {
            line_no: i + 1,
            reason: e,
        })?;
        out.push(event);
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn sanitize_app_id(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect()
}

fn default_egress_log_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("PF_EGRESS_LOG_DIR") {
        return PathBuf::from(v);
    }
    if let Some(base) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(base).join("pocketforge").join("egress");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local/state")
            .join("pocketforge")
            .join("egress");
    }
    std::env::temp_dir().join("pocketforge-egress")
}

// --- record encoding (matches AppOps dialect: tab-separated + backslash escapes) -------------

fn encode(event: &EgressEvent) -> String {
    let reason = event.reason.as_deref().unwrap_or("-");
    format!(
        "ts_ms={ts_ms}\tevent={event}\tapp={app}\thost={host}\tbytes={bytes}\tremaining_ops={remaining}\treason={reason}",
        ts_ms = event.ts_ms,
        event = event.event.to_str(),
        app = escape(&event.app_id),
        host = escape(&event.host),
        bytes = event.bytes,
        remaining = event.remaining_ops,
        reason = escape(reason),
    )
}

fn parse(line: &str) -> Result<EgressEvent, String> {
    let mut ts_ms: Option<u64> = None;
    let mut event: Option<EgressEventKind> = None;
    let mut app: Option<String> = None;
    let mut host: Option<String> = None;
    let mut bytes: Option<u64> = None;
    let mut remaining_ops: Option<u64> = None;
    let mut reason: Option<String> = None;

    for field in line.split('\t') {
        let (k, v) = field
            .split_once('=')
            .ok_or_else(|| format!("field '{field}' has no '='"))?;
        match k {
            "ts_ms" => ts_ms = Some(v.parse().map_err(|e| format!("ts_ms: {e}"))?),
            "event" => {
                event = Some(EgressEventKind::parse(v).ok_or_else(|| format!("bad event '{v}'"))?)
            }
            "app" => app = Some(unescape(v)?),
            "host" => host = Some(unescape(v)?),
            "bytes" => bytes = Some(v.parse().map_err(|e| format!("bytes: {e}"))?),
            "remaining_ops" => {
                remaining_ops = Some(v.parse().map_err(|e| format!("remaining_ops: {e}"))?)
            }
            "reason" => {
                let u = unescape(v)?;
                reason = if u == "-" { None } else { Some(u) };
            }
            _ => return Err(format!("unknown field '{k}'")),
        }
    }
    Ok(EgressEvent {
        ts_ms: ts_ms.ok_or("missing ts_ms")?,
        event: event.ok_or("missing event")?,
        app_id: app.ok_or("missing app")?,
        host: host.ok_or("missing host")?,
        bytes: bytes.ok_or("missing bytes")?,
        remaining_ops: remaining_ops.ok_or("missing remaining_ops")?,
        reason,
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

// --- ambient per-app tallies -----------------------------------------------------------------

/// Cheap in-memory helper used by the manager + `pf-permissions` to roll a per-host byte-total
/// out of a snapshot without re-reading files. Not persisted itself (rederivable from the log).
pub fn total_bytes_per_host(events: &[EgressEvent]) -> HashMap<String, u64> {
    let mut out: HashMap<String, u64> = HashMap::new();
    for e in events.iter().filter(|e| e.event == EgressEventKind::Send) {
        *out.entry(e.host.clone()).or_default() += e.bytes;
    }
    out
}

/// Count of `refused` events per host — surfaces attempted undeclared sends in
/// `pf-permissions egress --refusals`.
pub fn refusals_per_host(events: &[EgressEvent]) -> HashMap<String, u64> {
    let mut out: HashMap<String, u64> = HashMap::new();
    for e in events.iter().filter(|e| e.event == EgressEventKind::Refused) {
        *out.entry(e.host.clone()).or_default() += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "pf-egress-log-{}-{}-{}",
            tag,
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn send_and_refused_round_trip_through_the_log() {
        let dir = tmp_dir("round");
        let log = EgressLog::open(&dir).unwrap();
        log.record_send("com.test.round", "tile.example", 1500, 15).unwrap();
        log.record_refused(
            "com.test.round",
            "evil.example",
            "host evil.example not declared",
            15,
        )
        .unwrap();
        let events = log.snapshot_for("com.test.round").unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, EgressEventKind::Send);
        assert_eq!(events[0].host, "tile.example");
        assert_eq!(events[0].bytes, 1500);
        assert_eq!(events[1].event, EgressEventKind::Refused);
        assert_eq!(events[1].reason.as_deref(), Some("host evil.example not declared"));
    }

    #[test]
    fn totals_and_refusals_roll_up() {
        let dir = tmp_dir("rollup");
        let log = EgressLog::open(&dir).unwrap();
        log.record_send("com.test.roll", "a.host", 100, 15).unwrap();
        log.record_send("com.test.roll", "a.host", 200, 14).unwrap();
        log.record_send("com.test.roll", "b.host", 50, 13).unwrap();
        log.record_refused("com.test.roll", "z.host", "undeclared", 13).unwrap();
        log.record_refused("com.test.roll", "z.host", "undeclared", 13).unwrap();
        let all = log.snapshot_for("com.test.roll").unwrap();
        let totals = total_bytes_per_host(&all);
        assert_eq!(totals.get("a.host"), Some(&300));
        assert_eq!(totals.get("b.host"), Some(&50));
        assert_eq!(totals.get("z.host"), None, "refused rows do not add bytes");
        let refusals = refusals_per_host(&all);
        assert_eq!(refusals.get("z.host"), Some(&2));
    }

    #[test]
    fn escapes_survive_round_trip() {
        let dir = tmp_dir("escape");
        let log = EgressLog::open(&dir).unwrap();
        log.record_refused(
            "com.test.escape",
            "a\thost",
            "reason with \\ and \t and \n",
            0,
        )
        .unwrap();
        let events = log.snapshot_for("com.test.escape").unwrap();
        assert_eq!(events[0].host, "a\thost");
        assert_eq!(events[0].reason.as_deref(), Some("reason with \\ and \t and \n"));
    }

    #[test]
    fn malformed_line_stops_replay_with_line_number() {
        let dir = tmp_dir("bad");
        let path = dir.join("com.bad.log");
        std::fs::write(&path, "ts_ms=1\tevent=send\tapp=com.bad\thost=x\tbytes=1\tremaining_ops=0\treason=-\nbroken\n").unwrap();
        let log = EgressLog::open(&dir).unwrap();
        match log.snapshot_for("com.bad") {
            Err(EgressLogError::Malformed { line_no, .. }) => assert_eq!(line_no, 2),
            other => panic!("expected Malformed(2), got {other:?}"),
        }
    }
}
