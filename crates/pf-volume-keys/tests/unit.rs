//! The service unit is the host of the volume-key handler: it must be always-on (the input broker
//! only runs inside an app session, so a broker-hosted handler is dead while the shell or the
//! first-run sheet is up) and carry exactly the identity and device access the handler needs.

fn setting<'a>(unit: &'a str, key: &str) -> Vec<&'a str> {
    unit.lines()
        .filter_map(|l| l.strip_prefix(&format!("{key}=")))
        .collect()
}

const UNIT: &str = include_str!("../../../systemd/pf-volume-keys.service");

#[test]
fn unit_is_enabled_at_boot_and_runs_the_handler_as_the_prefs_writer_class() {
    assert_eq!(setting(UNIT, "WantedBy"), ["multi-user.target"]);
    assert_eq!(setting(UNIT, "ExecStart"), ["/usr/bin/pf-volume-keys"]);
    let groups = setting(UNIT, "SupplementaryGroups").join(" ");
    for needed in ["input", "audio", "pf-pref-writer"] {
        assert!(
            groups.split_whitespace().any(|g| g == needed),
            "missing {needed}: {groups}"
        );
    }
    assert!(setting(UNIT, "After")
        .join(" ")
        .contains("pf-prefsd.service"));
    assert_eq!(setting(UNIT, "Restart"), ["on-failure"]);
    assert_eq!(setting(UNIT, "DeviceAllow"), ["char-input r"]);
    // Negative control: the checker really sees a missing group.
    let stripped = UNIT.replace("pf-pref-writer", "");
    assert!(!setting(&stripped, "SupplementaryGroups")
        .join(" ")
        .contains("pf-pref-writer"));
}
