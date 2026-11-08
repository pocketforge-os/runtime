//! Fetch-destination policy tests (tsp-ziac.4.3.2).
//!
//! A `[fetch] destination = "var"` app has its payload installed at
//! `/var/lib/pocketforge/apps/<id>/<dest>`. The policy must grant that path
//! read-only and no-execute in Landlock, and the rendered drop-in must make it
//! read-only and hide `.pf-fetch` in the mount namespace. These tests pin the
//! whole before/after surface of that change.

use pf_app_launch::policy::{
    derive_policy, render_systemd_dropin, AppClass, FsAccess, PolicyInputs,
};
use pf_app_manifest::{parse_manifest, parse_platform_contract, ResolvedApp};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const MBHAXE_ID: &str = "org.pocketforge.mbhaxe";
const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn resolved(root: &Path, id: &str, capabilities: &[&str], fetch: &str) -> ResolvedApp {
    let capability_source = capabilities
        .iter()
        .map(|capability| format!("{capability:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let manifest = parse_manifest(&format!(
        "schema_version = 3\n\
         [app]\nid = {id:?}\nuse = [{capability_source}]\n\
         [runtime]\nfamily = \"pocketforge/a133-powervr\"\nabi = \"1\"\n\
         [launch]\nexec = \"bin/app\"\nneeds_network = false\n{fetch}"
    ))
    .unwrap();
    let supported = capabilities
        .iter()
        .filter_map(|token| {
            let token = token.strip_suffix('?').unwrap_or(token);
            let base = token.split_once(':').map_or(token, |(base, _)| base);
            (base != "egress").then_some(format!("{base:?}"))
        })
        .collect::<Vec<_>>()
        .join(", ");
    let platform = parse_platform_contract(&format!(
        "schema_version = 1\nruntime_family = \"pocketforge/a133-powervr\"\n\
         runtime_abi = \"1\"\nplatform_version = \"20\"\n\
         supported_capabilities = [{supported}]\nproduction = true\n\
         landlock_mode = \"required\"\nlandlock_full_min_abi = 10\n\
         landlock_browser_min_abi = 6\n"
    ))
    .unwrap();

    ResolvedApp {
        id: id.into(),
        app_dir: root.join("app"),
        executable: root.join("app/bin/app"),
        manifest,
        manifest_sha256: "fixture-manifest".into(),
        platform,
    }
}

fn inputs(root: &Path) -> PolicyInputs {
    PolicyInputs {
        platform_runtime: root.join("platform-runtime"),
        xdg_config: root.join("xdg/config/org.example.app"),
        xdg_state: root.join("xdg/state/org.example.app"),
        xdg_cache: root.join("xdg/cache/org.example.app"),
        resource_paths: vec![root.join("platform-dependencies")],
        resources: BTreeMap::new(),
    }
}

fn policy(
    root: &Path,
    id: &str,
    capabilities: &[&str],
    fetch: &str,
) -> pf_app_launch::policy::PolicyDecision {
    derive_policy(&resolved(root, id, capabilities, fetch), &inputs(root)).unwrap()
}

fn fetch_file(dest: &str) -> String {
    format!(
        "\n[[fetch.files]]\nurl = \"https://example.invalid/data.tar.gz\"\n\
         sha256 = \"{SHA}\"\nsize = 1024\nformat = \"tar.gz\"\ndest = {dest:?}\n"
    )
}

fn enabled_var_fetch(files: &[String]) -> String {
    format!(
        "\n[fetch]\nenabled = true\nreason = \"fixture\"\ndestination = \"var\"\n{}",
        files.concat()
    )
}

fn mbhaxe_fetch() -> String {
    enabled_var_fetch(&[fetch_file("data")])
}

fn destination(id: &str, dest: &str) -> PathBuf {
    PathBuf::from("/var/lib/pocketforge/apps")
        .join(id)
        .join(dest)
}

/// Return the lines `after` adds to `before`. Fails if any line is removed or
/// reordered, so the caller asserts the *full* before/after diff.
fn added_lines(before: &str, after: &str) -> Vec<String> {
    let before: Vec<&str> = before.lines().collect();
    let mut index = 0;
    let mut added = Vec::new();
    for line in after.lines() {
        if before.get(index) == Some(&line) {
            index += 1;
        } else {
            added.push(line.to_owned());
        }
    }
    assert_eq!(
        index,
        before.len(),
        "the fetch change must not remove, reorder or alter any existing line"
    );
    added
}

#[test]
fn mbhaxe_shaped_fetch_grants_one_read_only_destination_rule() {
    let root = PathBuf::from("/fixture-fetch");
    let expected = destination(MBHAXE_ID, "data");
    let base = policy(&root, MBHAXE_ID, &[], "");
    let fetched = policy(&root, MBHAXE_ID, &[], &mbhaxe_fetch());

    assert!(!base
        .landlock
        .filesystem
        .iter()
        .any(|rule| rule.path == expected));
    let rules: Vec<_> = fetched
        .landlock
        .filesystem
        .iter()
        .filter(|rule| rule.path == expected)
        .collect();
    assert_eq!(rules.len(), 1, "exactly one rule for the fetch destination");
    assert_eq!(
        rules[0].access,
        FsAccess::ReadOnly,
        "read file and read dir only: no write, execute, make-*, remove or truncate"
    );
}

#[test]
fn mbhaxe_shaped_dropin_adds_exactly_the_fetch_mount_lines() {
    let root = PathBuf::from("/fixture-fetch");
    let base = policy(&root, MBHAXE_ID, &[], "");
    let fetched = policy(&root, MBHAXE_ID, &[], &mbhaxe_fetch());

    let before = render_systemd_dropin(&base);
    let after = render_systemd_dropin(&fetched);
    assert_eq!(
        added_lines(&before, &after),
        vec![
            format!(
                "InaccessiblePaths=-{}",
                destination(MBHAXE_ID, ".pf-fetch").display()
            ),
            format!(
                "ReadOnlyPaths=-{}",
                destination(MBHAXE_ID, "data").display()
            ),
        ],
        "the fetch change adds exactly the read-only dest line and the hidden receipt/staging dir"
    );

    let read_only = format!(
        "ReadOnlyPaths=-{}",
        destination(MBHAXE_ID, "data").display()
    );
    let inaccessible = format!(
        "InaccessiblePaths=-{}",
        destination(MBHAXE_ID, ".pf-fetch").display()
    );
    assert!(after.contains(&format!("{read_only}\n")));
    assert!(after.contains(&format!("{inaccessible}\n")));
    assert_eq!(after.lines().filter(|line| *line == read_only).count(), 1);
    assert_eq!(
        after.lines().filter(|line| *line == inaccessible).count(),
        1
    );
    assert!(!before.contains(".pf-fetch"));
}

#[test]
fn nested_userns_fetch_gets_the_same_mount_lines_without_landlock_rules() {
    let root = PathBuf::from("/fixture-fetch");
    let id = "org.example.browser";
    let base = policy(&root, id, &["sandbox.nested_userns"], "");
    let fetched = policy(&root, id, &["sandbox.nested_userns"], &mbhaxe_fetch());

    assert_eq!(fetched.class, AppClass::NestedUserns);
    assert!(
        fetched.landlock.filesystem.is_empty(),
        "the NestedUserns class intentionally has no Landlock filesystem rules"
    );

    let before = render_systemd_dropin(&base);
    let after = render_systemd_dropin(&fetched);
    assert_eq!(
        added_lines(&before, &after),
        vec![
            format!(
                "InaccessiblePaths=-{}",
                destination(id, ".pf-fetch").display()
            ),
            format!("ReadOnlyPaths=-{}", destination(id, "data").display()),
        ],
        "the mount-namespace lines are the only enforcement a NestedUserns app gets"
    );
}

#[test]
fn no_fetch_and_disabled_fetch_are_byte_identical_to_the_base() {
    let root = PathBuf::from("/fixture-fetch");
    let disabled = format!(
        "\n[fetch]\nenabled = false\ndestination = \"var\"\n{}",
        fetch_file("data")
    );
    let none = policy(&root, MBHAXE_ID, &[], "");
    let off = policy(&root, MBHAXE_ID, &[], &disabled);

    assert_eq!(none, off);
    assert_eq!(
        serde_json::to_string(&none).unwrap(),
        serde_json::to_string(&off).unwrap(),
        "the policy JSON is byte-identical, not merely equivalent"
    );
    assert_eq!(
        render_systemd_dropin(&none),
        render_systemd_dropin(&off),
        "the drop-in is byte-identical"
    );
    assert!(!render_systemd_dropin(&off).contains(".pf-fetch"));
}

#[test]
fn shared_destinations_are_deduplicated_and_distinct_destinations_stack() {
    let root = PathBuf::from("/fixture-fetch");

    let shared = policy(
        &root,
        MBHAXE_ID,
        &[],
        &enabled_var_fetch(&[fetch_file("data"), fetch_file("data")]),
    );
    let rules: Vec<_> = shared
        .landlock
        .filesystem
        .iter()
        .filter(|rule| rule.path == destination(MBHAXE_ID, "data"))
        .collect();
    assert_eq!(rules.len(), 1, "two files sharing a dest produce one rule");
    let rendered = render_systemd_dropin(&shared);
    assert_eq!(
        rendered
            .lines()
            .filter(|line| *line
                == format!(
                    "ReadOnlyPaths=-{}",
                    destination(MBHAXE_ID, "data").display()
                ))
            .count(),
        1
    );

    let distinct = policy(
        &root,
        MBHAXE_ID,
        &[],
        &enabled_var_fetch(&[fetch_file("data"), fetch_file("assets")]),
    );
    for dest in ["data", "assets"] {
        let rules: Vec<_> = distinct
            .landlock
            .filesystem
            .iter()
            .filter(|rule| rule.path == destination(MBHAXE_ID, dest))
            .collect();
        assert_eq!(rules.len(), 1, "one rule for dest {dest}");
        assert_eq!(rules[0].access, FsAccess::ReadOnly);
    }
    let rendered = render_systemd_dropin(&distinct);
    for dest in ["data", "assets"] {
        assert!(rendered.contains(&format!(
            "ReadOnlyPaths=-{}\n",
            destination(MBHAXE_ID, dest).display()
        )));
    }
}

#[test]
fn unsafe_fetch_destinations_are_refused_at_derive_time() {
    let root = PathBuf::from("/fixture-fetch");
    let base = resolved(&root, MBHAXE_ID, &[], &mbhaxe_fetch());

    for (label, dest) in [
        ("missing", None),
        ("empty", Some("")),
        ("dot", Some(".")),
        ("dotdot", Some("..")),
        ("nested", Some("a/b")),
        ("config", Some("config")),
        ("state", Some("state")),
        ("cache", Some("cache")),
    ] {
        let mut app = base.clone();
        app.manifest.fetch.as_mut().unwrap().files.as_mut().unwrap()[0].dest =
            dest.map(str::to_owned);
        let error = derive_policy(&app, &inputs(&root))
            .expect_err(&format!("dest {label:?} must be refused"))
            .to_string();
        assert!(
            error.contains("invalid fetch dest"),
            "dest {label:?} error is clear: {error}"
        );
    }
}

#[test]
fn non_var_destinations_get_no_read_only_grant_but_still_hide_fetch_state() {
    let root = PathBuf::from("/fixture-fetch");
    let home = format!(
        "\n[fetch]\nenabled = true\nreason = \"fixture\"\ndestination = \"home\"\n{}",
        fetch_file("data")
    );
    let policy = policy(&root, MBHAXE_ID, &[], &home);
    let dest = destination(MBHAXE_ID, "data");

    assert!(!policy
        .landlock
        .filesystem
        .iter()
        .any(|rule| rule.path == dest));
    let rendered = render_systemd_dropin(&policy);
    assert!(!rendered.contains(&format!("ReadOnlyPaths=-{}", dest.display())));
    assert!(rendered.contains(&format!(
        "InaccessiblePaths=-{}\n",
        destination(MBHAXE_ID, ".pf-fetch").display()
    )));
}
