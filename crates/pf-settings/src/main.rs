//! `pf-settings` — the v1 **writer surface** for user/accessibility preferences (owner ruling
//! Q1), the authority-side counterpart to the read-only-to-apps contract.
//!
//! Modeled on `pf-permissions` (the AppOps inspect/revoke CLI): a small standalone tool a
//! settings screen — or an operator on a serial console — uses to read and change preferences.
//! Every write goes through the single [`pf_prefs::PrefsStore`] persist-and-signal seam, the same
//! path the on-panel settings UI (`.3`) and the supervisor will use later. Apps never link this;
//! they read the store through the capability facade and are read-only on it by contract.
//!
//! The store root is discovered from `$PF_PREFS_DIR` (else `$XDG_STATE_HOME/pocketforge/prefs`,
//! else `$HOME/.local/state/pocketforge/prefs`) — see [`pf_prefs::PrefsStore`].
//!
//! Usage:
//!   pf-settings get  <key>
//!   pf-settings set  <key> <value>
//!   pf-settings list

use pf_prefs::{parse_value, PrefsStore, Source, SCHEMA};

const USAGE: &str = "\
pf-settings — read/change PocketForge user & accessibility preferences

USAGE:
    pf-settings get  <key>          Print the effective value of a preference
    pf-settings set  <key> <value>  Set a preference (validated, atomically persisted)
    pf-settings list                Show every preference: type, value, default, source

Store: $PF_PREFS_DIR/prefs.json (else $XDG_STATE_HOME/.../prefs, else ~/.local/state/.../prefs)
Preferences are READ-ONLY TO APPS by contract; this is the authority-side writer.";

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
        "get" => run_get(rest),
        "set" => run_set(rest),
        "list" => run_list(rest),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        _ => {
            eprintln!("pf-settings: unknown subcommand '{sub}'\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("pf-settings: {e}");
        std::process::exit(1);
    }
}

fn run_get(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let key = args.first().ok_or("get requires a <key> (see `pf-settings list`)")?;
    let prefs = PrefsStore::open_default().load()?;
    let value = prefs.value(key)?;
    println!("{value}");
    Ok(())
}

fn run_set(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let key = args.first().ok_or("set requires a <key> and <value>")?;
    let raw = args.get(1).ok_or("set requires a <value>")?;
    let value = parse_value(key, raw)?;
    let store = PrefsStore::open_default();
    match store.apply(key, value)? {
        Some(change) => println!("{}: {} -> {}", change.key, change.old, change.new),
        None => println!("{key}: {value} (unchanged)"),
    }
    Ok(())
}

fn run_list(_args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let prefs = PrefsStore::open_default().load()?;
    println!("{:<16} {:<8} {:<8} {:<8} {:<8}  description", "key", "type", "value", "default", "source");
    for spec in SCHEMA {
        let value = prefs.value(spec.key)?;
        let source = match prefs.source(spec.key) {
            Source::Default => "default",
            Source::Stored => "stored",
        };
        let ty = match value {
            pf_prefs::PrefValue::Bool(_) => "bool",
            pf_prefs::PrefValue::Scalar(_) => "scalar",
        };
        println!(
            "{:<16} {:<8} {:<8} {:<8} {:<8}  {}",
            spec.key,
            ty,
            value.to_string(),
            spec.default.to_string(),
            source,
            spec.doc,
        );
    }
    Ok(())
}
