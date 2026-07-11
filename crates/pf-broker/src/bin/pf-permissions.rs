//! `pf-permissions` — the OUT-OF-BAND inspect + revoke surface for the AppOps ledger
//! (`tsp-ht0p.3`).
//!
//! Modeled on Android's `pm reset-permissions` / the `flatpak permissions` view: a small
//! standalone CLI that a settings screen (or an operator on a serial console) uses to see the
//! grants an app currently holds, and to revoke one on demand. Revocation writes a `revoke`
//! event to the persistent ledger — the resurrection-hazard fix (see [`pf_broker::appops`]).
//! A running broker's InProcessBackend does NOT observe this write directly (the CLI is a
//! separate process); in-process test flows fire the change event via a shared
//! `InProcessBackend` (proven in `tests/consent_flow.rs`), and the substrate-era supervisor will
//! carry a live signal from the ledger to any running app. Until then, an app re-queries on next
//! interaction and sees the standing-Denied.
//!
//! ## `.4` addition — egress accounting inspection
//!
//! The `egress` subcommand reads the [`pocketforge::managers::EgressLog`] (same JSONL dialect
//! as the AppOps ledger; separate directory + file so a consumer never conflates a consent
//! grant with a byte-accounting event) and prints a per-`(app × host)` rollup of accounted
//! bytes + refusal counts. This is the surface an operator uses to sanity-check the
//! declaration + accounting side of the E3 policy model (`tsp-ht0p.4`); real ENFORCEMENT is
//! substrate-gated (see `docs/EGRESS-ENFORCEMENT-SEAM.md`).
//!
//! Usage:
//!   pf-permissions inspect [--app <id>]
//!   pf-permissions revoke  --app <id> --cap <name> [--modifier <mod>]
//!   pf-permissions egress  [--app <id>] [--refusals]
//!
//! The ledger root is discovered from `$PF_APPOPS_DIR` (else `$XDG_STATE_HOME`, else
//! `$HOME/.local/state/pocketforge/appops`) — see [`pf_broker::appops::AppOpsLedger`]. The
//! egress log root uses `$PF_EGRESS_LOG_DIR` (else `$XDG_STATE_HOME`/pocketforge/egress).

use pf_broker::appops::{AppOpsLedger, GrantKey};
use pocketforge::managers::egress_log::{refusals_per_host, total_bytes_per_host, EgressLog};

fn arg<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (sub, rest) = match args.split_first() {
        Some(s) => s,
        None => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    let result = match sub.as_str() {
        "inspect" => run_inspect(rest),
        "revoke" => run_revoke(rest),
        "egress" => run_egress(rest),
        "-h" | "--help" => {
            println!("{USAGE}");
            Ok(())
        }
        _ => {
            eprintln!("pf-permissions: unknown subcommand '{sub}'\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("pf-permissions: {e}");
        std::process::exit(1);
    }
}

fn run_inspect(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let app_filter = arg(args, "--app");
    let ledger = AppOpsLedger::open_default()?;
    println!(
        "app.id                          cap                  modifier                      scope   ts_ms          op_id  input          note"
    );
    for g in ledger.snapshot() {
        if let Some(f) = app_filter {
            if g.key.app_id != f {
                continue;
            }
        }
        println!(
            "{app:31} {cap:20} {modifier:29} {scope:7} {ts:14} {op:6} {input:14} {note}",
            app = truncate(&g.key.app_id, 31),
            cap = g.key.cap,
            modifier = g.key.modifier.as_deref().unwrap_or("-"),
            scope = match g.scope {
                pf_broker::appops::Scope::Once => "once",
                pf_broker::appops::Scope::Always => "always",
            },
            ts = g.ts_ms,
            op = g.op_id,
            input = g.input.map(|i| i.as_str()).unwrap_or("-"),
            note = g.supervisor_note.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}

fn run_revoke(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let app_id = arg(args, "--app").ok_or("revoke: --app <id> is required")?;
    let cap = arg(args, "--cap").ok_or("revoke: --cap <name> is required")?;
    let modifier = arg(args, "--modifier");
    let ledger = AppOpsLedger::open_default()?;
    let key = GrantKey::new(app_id, cap, modifier);
    ledger.record_revoke(&key)?;
    println!(
        "revoked: app={} cap={} modifier={}",
        app_id,
        cap,
        modifier.unwrap_or("-")
    );
    Ok(())
}

fn run_egress(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    // `pf-permissions egress [--app <id>] [--refusals]` — accounting-only surface (`tsp-ht0p.4`,
    // Q1 = ACCOUNTING ONLY in v1). The default view is a per-`(app × host)` byte rollup; the
    // `--refusals` flag surfaces attempted undeclared sends instead. R-A: neither view claims
    // enforcement — the seam that closes the gap is `docs/EGRESS-ENFORCEMENT-SEAM.md`.
    let app_filter = arg(args, "--app");
    let show_refusals = args.iter().any(|a| a == "--refusals");
    let log = EgressLog::open_default()?;
    let events = match app_filter {
        Some(app) => log.snapshot_for(app)?,
        None => log.snapshot()?,
    };
    if show_refusals {
        println!("app.id                          host                          refusals");
        // Refusals are per-app × per-host; walk grouped by app id.
        let mut apps: std::collections::BTreeMap<String, Vec<&pocketforge::managers::EgressEvent>> =
            std::collections::BTreeMap::new();
        for e in &events {
            apps.entry(e.app_id.clone()).or_default().push(e);
        }
        for (app, es) in apps {
            let refs = refusals_per_host(&es.iter().map(|&e| e.clone()).collect::<Vec<_>>());
            let mut sorted: Vec<(&String, &u64)> = refs.iter().collect();
            sorted.sort();
            for (host, count) in sorted {
                println!(
                    "{app:31} {host:29} {count:>8}",
                    app = truncate(&app, 31),
                    host = host,
                    count = count,
                );
            }
        }
    } else {
        println!(
            "app.id                          host                          bytes_total    events"
        );
        // Grouped by app then per-host bytes.
        let mut apps: std::collections::BTreeMap<String, Vec<&pocketforge::managers::EgressEvent>> =
            std::collections::BTreeMap::new();
        for e in &events {
            apps.entry(e.app_id.clone()).or_default().push(e);
        }
        for (app, es) in apps {
            let bytes = total_bytes_per_host(&es.iter().map(|&e| e.clone()).collect::<Vec<_>>());
            let mut sorted: Vec<(&String, &u64)> = bytes.iter().collect();
            sorted.sort();
            for (host, total) in sorted {
                let event_count = es.iter().filter(|e| &e.host == host).count();
                println!(
                    "{app:31} {host:29} {total:>13}    {events:>6}",
                    app = truncate(&app, 31),
                    host = host,
                    total = total,
                    events = event_count,
                );
            }
        }
    }
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n - 1])
    }
}

const USAGE: &str = "\
pf-permissions — inspect + revoke AppOps ledger grants; inspect egress accounting (tsp-ht0p.3/.4)

Usage:
  pf-permissions inspect [--app <id>]
  pf-permissions revoke  --app <id> --cap <name> [--modifier <mod>]
  pf-permissions egress  [--app <id>] [--refusals]     # v1 accounting-only view (tsp-ht0p.4)
";
