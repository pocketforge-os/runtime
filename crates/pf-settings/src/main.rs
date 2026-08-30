//! `pf-settings` — the v1 **writer surface** for user/accessibility preferences (owner ruling
//! Q1), the authority-side counterpart to the read-only-to-apps contract.
//!
//! Modeled on `pf-permissions` (the AppOps inspect/revoke CLI): a small standalone tool a
//! settings screen — or an operator on a serial console — uses to read and change preferences.
//! When `$PF_PREFSD_SOCK` is set, reads and writes go through that daemon; otherwise the CLI uses
//! the existing [`pf_prefs::PrefsStore`] path. Apps never link this; they read preferences through
//! the capability facade and are read-only on them by contract.
//!
//! The store root is discovered from `$PF_PREFS_DIR` (else `$XDG_STATE_HOME/pocketforge/prefs`,
//! else `$HOME/.local/state/pocketforge/prefs`) — see [`pf_prefs::PrefsStore`].
//!
//! Usage:
//!   pf-settings get  <key>
//!   pf-settings set  <key> <value>
//!   pf-settings list

use pf_prefs::{parse_value, PrefsStore, Source, SCHEMA};
use pf_prefsd::Client;

const USAGE: &str = "\
pf-settings — read/change PocketForge user & accessibility preferences

USAGE:
    pf-settings get  <key>          Print the effective value of a preference
    pf-settings set  <key> <value>  Set a preference (validated, atomically persisted)
    pf-settings list                Show every preference: type, value, default, source

Backend: $PF_PREFSD_SOCK when set; otherwise $PF_PREFS_DIR/prefs.json
(else $XDG_STATE_HOME/.../prefs, else ~/.local/state/.../prefs)
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
    let key = args
        .first()
        .ok_or("get requires a <key> (see `pf-settings list`)")?;
    if let Some(client) = daemon_client() {
        println!("{}", display_json(client.get(key)?));
    } else {
        let prefs = PrefsStore::open_default().load()?;
        println!("{}", prefs.value(key)?);
    }
    Ok(())
}

fn run_set(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let key = args.first().ok_or("set requires a <key> and <value>")?;
    let raw = args.get(1).ok_or("set requires a <value>")?;
    let value = parse_value(key, raw)?;
    if let Some(client) = daemon_client() {
        let old = client.get(key)?;
        let new = client.set(key, pref_to_json(value))?;
        if old == new {
            println!("{key}: {} (unchanged)", display_json(new));
        } else {
            println!("{key}: {} -> {}", display_json(old), display_json(new));
        }
        return Ok(());
    }
    let store = PrefsStore::open_default();
    match store.apply(key, value)? {
        Some(change) => println!("{}: {} -> {}", change.key, change.old, change.new),
        None => println!("{key}: {value} (unchanged)"),
    }
    Ok(())
}

fn run_list(_args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let daemon_values = daemon_client().map(|client| client.get_all()).transpose()?;
    let prefs = if daemon_values.is_none() {
        Some(PrefsStore::open_default().load()?)
    } else {
        None
    };
    println!(
        "{:<16} {:<8} {:<8} {:<8} {:<8}  description",
        "key", "type", "value", "default", "source"
    );
    for spec in SCHEMA {
        let value = match &daemon_values {
            Some(values) => display_json(
                values
                    .get(spec.key)
                    .ok_or_else(|| format!("daemon omitted preference '{}'", spec.key))?
                    .clone(),
            ),
            None => prefs
                .as_ref()
                .expect("local prefs")
                .value(spec.key)?
                .to_string(),
        };
        let source = match &daemon_values {
            Some(_) => "daemon",
            None => match prefs.as_ref().expect("local prefs").source(spec.key) {
                Source::Default => "default",
                Source::Stored => "stored",
            },
        };
        let ty = match spec.kind {
            pf_prefs::PrefKind::Bool => "bool",
            pf_prefs::PrefKind::Scalar { .. } => "scalar",
            pf_prefs::PrefKind::Enum { .. } => "enum",
        };
        println!(
            "{:<16} {:<8} {:<8} {:<8} {:<8}  {}",
            spec.key,
            ty,
            value,
            spec.default.to_string(),
            source,
            spec.doc,
        );
    }
    Ok(())
}

fn daemon_client() -> Option<Client> {
    std::env::var_os("PF_PREFSD_SOCK").map(Client::new)
}

fn pref_to_json(value: pf_prefs::PrefValue) -> serde_json::Value {
    match value {
        pf_prefs::PrefValue::Bool(value) => value.into(),
        pf_prefs::PrefValue::Scalar(value) => value.into(),
        pf_prefs::PrefValue::Enum(value) => value.into(),
    }
}

fn display_json(value: serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value,
        value => value.to_string(),
    }
}
