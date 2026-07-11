//! End-to-end CLI test: `set` then `get` round-trips through a scratch `PF_PREFS_DIR`, and
//! `list` reports typed values + defaults + source. Exercises the built `pf-settings` binary.

use std::path::PathBuf;
use std::process::Command;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pf-settings-cli-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn run(dir: &PathBuf, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pf-settings"))
        .args(args)
        .env("PF_PREFS_DIR", dir)
        // Neutralize ambient env so the override is the only source that resolves.
        .env_remove("XDG_STATE_HOME")
        .output()
        .expect("run pf-settings")
}

#[test]
fn set_then_get_round_trips() {
    let dir = scratch("roundtrip");

    let set = run(&dir, &["set", "hapticsEnabled", "false"]);
    assert!(set.status.success(), "set failed: {}", String::from_utf8_lossy(&set.stderr));

    let get = run(&dir, &["get", "hapticsEnabled"]);
    assert!(get.status.success());
    assert_eq!(String::from_utf8_lossy(&get.stdout).trim(), "false");

    // The stored document is a real, inspectable JSON file.
    let doc = std::fs::read_to_string(dir.join("prefs.json")).unwrap();
    assert!(doc.contains("\"hapticsEnabled\""));
    assert!(doc.contains("false"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn get_unset_key_returns_default() {
    let dir = scratch("default");
    let get = run(&dir, &["get", "brightness"]);
    assert!(get.status.success());
    assert_eq!(String::from_utf8_lossy(&get.stdout).trim(), "100");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn list_shows_values_defaults_and_source() {
    let dir = scratch("list");
    run(&dir, &["set", "brightness", "40"]);
    let list = run(&dir, &["list"]);
    assert!(list.status.success());
    let out = String::from_utf8_lossy(&list.stdout);
    assert!(out.contains("brightness"));
    assert!(out.contains("stored")); // brightness was set
    assert!(out.contains("default")); // the untouched prefs
    assert!(out.contains("hapticsEnabled"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn set_rejects_out_of_range_and_unknown() {
    let dir = scratch("reject");
    let oor = run(&dir, &["set", "brightness", "250"]);
    assert!(!oor.status.success());
    let unknown = run(&dir, &["set", "nopeKey", "true"]);
    assert!(!unknown.status.success());
    let _ = std::fs::remove_dir_all(&dir);
}
