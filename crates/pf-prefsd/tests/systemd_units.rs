use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

fn service_files(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).expect("read source directory") {
        let path = entry.expect("read source entry").path();
        if path.is_dir() {
            let name = path.file_name().and_then(|name| name.to_str());
            if !matches!(name, Some(".git" | "target" | "vendor")) {
                service_files(&path, files);
            }
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("service") {
            files.push(path);
        }
    }
}

fn runtime_directory_conflicts<'a>(
    units: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<String> {
    let mut declarations: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();

    for (name, contents) in units {
        let mut section = "";
        let mut user = "root";
        let mut directories = Vec::new();
        for raw_line in contents.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with(['#', ';']) {
                continue;
            }
            if let Some(section_name) = line
                .strip_prefix('[')
                .and_then(|line| line.strip_suffix(']'))
            {
                section = section_name;
                continue;
            }
            if section != "Service" {
                continue;
            }
            if let Some(value) = line.strip_prefix("User=") {
                user = if value.is_empty() { "root" } else { value };
            } else if let Some(value) = line.strip_prefix("RuntimeDirectory=") {
                directories.extend(value.split_whitespace());
            }
        }
        for directory in directories {
            declarations
                .entry(directory)
                .or_default()
                .push((user, name));
        }
    }

    declarations
        .into_iter()
        .filter_map(|(directory, owners)| {
            let users: BTreeSet<_> = owners.iter().map(|(user, _)| *user).collect();
            (users.len() > 1).then(|| {
                let detail = owners
                    .iter()
                    .map(|(user, name)| format!("{name} (User={user})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("RuntimeDirectory={directory}: {detail}")
            })
        })
        .collect()
}

fn service_setting<'a>(unit: &'a str, key: &str) -> Option<&'a str> {
    let mut section = "";
    unit.lines().find_map(|raw_line| {
        let line = raw_line.trim();
        if let Some(section_name) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            section = section_name;
            return None;
        }
        (section == "Service")
            .then(|| line.strip_prefix(key))
            .flatten()
            .and_then(|value| value.strip_prefix('='))
    })
}

fn volume_reader_identity_error(prefsd: &str, volumed: &str) -> Option<String> {
    let prefs_owner = service_setting(prefsd, "User").unwrap_or("root");
    let volume_user = service_setting(volumed, "User").unwrap_or("root");
    if volume_user != prefs_owner {
        return Some(format!(
            "pf-volumed User={volume_user} cannot read as pf-prefsd owner User={prefs_owner}"
        ));
    }
    let prefs_group = service_setting(prefsd, "Group").unwrap_or("root");
    let volume_group = service_setting(volumed, "Group").unwrap_or("root");
    if volume_group != prefs_group {
        return Some(format!(
            "pf-volumed Group={volume_group} must remain outside the writer group"
        ));
    }
    let groups = service_setting(volumed, "SupplementaryGroups").unwrap_or("");
    if !groups.split_whitespace().any(|group| group == "audio") {
        return Some(String::from(
            "pf-volumed needs SupplementaryGroups=audio for the ALSA devices",
        ));
    }
    if groups
        .split_whitespace()
        .any(|group| group == "pf-pref-writer")
    {
        return Some(String::from(
            "pf-volumed must remain a read-only preference client",
        ));
    }
    None
}

#[test]
fn units_do_not_share_runtime_directories_across_users() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut paths = Vec::new();
    service_files(&root, &mut paths);
    paths.sort();
    let contents: Vec<_> = paths
        .iter()
        .map(|path| fs::read_to_string(path).expect("read systemd service"))
        .collect();
    let units = paths
        .iter()
        .zip(&contents)
        .map(|(path, contents)| (path.to_str().expect("UTF-8 path"), contents.as_str()));

    let conflicts = runtime_directory_conflicts(units);

    assert!(conflicts.is_empty(), "{}", conflicts.join("\n"));
}

#[test]
fn different_users_are_detected() {
    let conflicts = runtime_directory_conflicts([
        (
            "first.service",
            "[Service]\nUser=gamer\nRuntimeDirectory=shared\n",
        ),
        (
            "second.service",
            "[Service]\nUser=root\nRuntimeDirectory=shared\n",
        ),
    ]);

    assert_eq!(conflicts.len(), 1);
    assert!(conflicts[0].contains("RuntimeDirectory=shared"));
}

#[test]
fn missing_user_is_treated_as_root() {
    let conflicts = runtime_directory_conflicts([
        (
            "gamer.service",
            "[Service]\nUser=gamer\nRuntimeDirectory=shared\n",
        ),
        (
            "default-root.service",
            "[Service]\nRuntimeDirectory=shared\n",
        ),
    ]);

    assert_eq!(conflicts.len(), 1);
    assert!(conflicts[0].contains("gamer.service (User=gamer)"));
    assert!(conflicts[0].contains("default-root.service (User=root)"));
}

#[test]
fn volume_service_uses_the_read_only_prefs_owner_for_startup_and_polling() {
    let prefsd = include_str!("../../../systemd/pf-prefsd.service");
    let volumed = include_str!("../../../systemd/pf-volumed.service");

    assert_eq!(
        volume_reader_identity_error(prefsd, "[Service]\nUser=root\nSupplementaryGroups=audio\n")
            .as_deref(),
        Some("pf-volumed User=root cannot read as pf-prefsd owner User=gamer")
    );
    assert_eq!(
        volume_reader_identity_error(
            prefsd,
            "[Service]\nUser=gamer\nGroup=pf-pref-writer\nSupplementaryGroups=audio\n"
        )
        .as_deref(),
        Some("pf-volumed Group=pf-pref-writer must remain outside the writer group")
    );
    assert_eq!(
        volume_reader_identity_error(prefsd, "[Service]\nUser=gamer\nGroup=gamer\n").as_deref(),
        Some("pf-volumed needs SupplementaryGroups=audio for the ALSA devices")
    );
    assert_eq!(
        volume_reader_identity_error(
            prefsd,
            "[Service]\nUser=gamer\nGroup=gamer\nSupplementaryGroups=audio pf-pref-writer\n"
        )
        .as_deref(),
        Some("pf-volumed must remain a read-only preference client")
    );
    assert_eq!(volume_reader_identity_error(prefsd, volumed), None);
}
