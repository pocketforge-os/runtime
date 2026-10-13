use pf_app_launch::landlock::{apply_landlock, EffectiveRuleset, LandlockKernel, SystemLandlock};
use pf_app_launch::policy::{
    derive_policy, AppClass, DowngradeMode, NetAccess, NetRule, PolicyInputs,
};
use pf_app_manifest::{parse_manifest, parse_platform_contract, ResolvedApp};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("pf-app-landlock-{name}-{}", std::process::id()))
}

fn policy(root: &Path, production: bool) -> pf_app_launch::policy::PolicyDecision {
    let mode = if production {
        "required"
    } else {
        "best_effort"
    };
    let manifest = parse_manifest(
        "[app]\nid = \"org.example.app\"\nuse = []\n\
         [runtime]\nfamily = \"pocketforge/a133-powervr\"\nabi = \"1\"\n\
         [launch]\nexec = \"bin/app\"\n",
    )
    .unwrap();
    let platform = parse_platform_contract(&format!(
        "schema_version = 1\nruntime_family = \"pocketforge/a133-powervr\"\n\
         runtime_abi = \"1\"\nplatform_version = \"20\"\nsupported_capabilities = []\n\
         production = {production}\nlandlock_mode = \"{mode}\"\n\
         landlock_full_min_abi = 10\nlandlock_browser_min_abi = 6\n"
    ))
    .unwrap();
    let resolved = ResolvedApp {
        id: "org.example.app".into(),
        app_dir: root.join("app"),
        executable: root.join("app/bin/app"),
        manifest,
        manifest_sha256: "fixture-manifest".into(),
        platform,
    };
    derive_policy(
        &resolved,
        &PolicyInputs {
            platform_runtime: root.join("runtime"),
            xdg_config: root.join("own/config"),
            xdg_state: root.join("own/state"),
            xdg_cache: root.join("own/cache"),
            resource_paths: Vec::new(),
            resources: BTreeMap::new(),
        },
    )
    .unwrap()
}

#[test]
fn full_profile_denies_cross_app_state_with_eacces() {
    const INNER: &str = "PF_LANDLOCK_TEST_INNER";
    if std::env::var_os(INNER).is_none() {
        let root = scratch("syscall");
        fs::create_dir_all(&root).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "full_profile_denies_cross_app_state_with_eacces",
                "--nocapture",
            ])
            .env(INNER, &root)
            .output()
            .unwrap();
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        assert!(
            output.status.success(),
            "child stdout:\n{}\nchild stderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        fs::remove_file(root.join("app/bin/app")).unwrap();
        fs::remove_file(root.join("runtime/runtime.txt")).unwrap();
        fs::remove_file(root.join("own/state/created")).unwrap();
        fs::remove_file(root.join("foreign/state/sentinel")).unwrap();
        for directory in [
            root.join("app/bin"),
            root.join("app"),
            root.join("runtime"),
            root.join("own/config"),
            root.join("own/state"),
            root.join("own/cache"),
            root.join("own"),
            root.join("foreign/state"),
            root.join("foreign"),
            root.join("seeded-violation"),
            root.clone(),
        ] {
            fs::remove_dir(directory).unwrap();
        }
        return;
    }

    let root = PathBuf::from(std::env::var_os(INNER).unwrap());
    for directory in [
        root.join("app/bin"),
        root.join("runtime"),
        root.join("own/config"),
        root.join("own/state"),
        root.join("own/cache"),
        root.join("foreign/state"),
        root.join("seeded-violation"),
    ] {
        fs::create_dir_all(directory).unwrap();
    }
    fs::write(root.join("app/bin/app"), b"app").unwrap();
    fs::write(root.join("runtime/runtime.txt"), b"runtime").unwrap();
    fs::write(root.join("foreign/state/sentinel"), b"foreign-secret").unwrap();

    // Negative-control setup: the same uid can read the complete sentinel before Landlock.
    assert_eq!(
        fs::read(root.join("foreign/state/sentinel")).unwrap(),
        b"foreign-secret"
    );
    let mut kernel = SystemLandlock;
    let abi = kernel.abi().expect("query Landlock ABI");
    eprintln!("landlock_test_abi={abi} declared_floor=4");
    assert!(
        abi >= 4,
        "runner Landlock ABI {abi} is below declared CI floor 4"
    );
    let applied = apply_landlock(&policy(&root, false), &mut kernel).unwrap();
    assert_eq!(applied.kernel_abi, abi);

    // Positive controls remain usable under the supported ABI subset.
    assert_eq!(
        fs::read(root.join("runtime/runtime.txt")).unwrap(),
        b"runtime"
    );
    fs::write(root.join("own/state/created"), b"ok").unwrap();

    // Both real foreign state and a seeded omitted hierarchy fail with exact EACCES.
    for denied in [
        root.join("foreign/state/sentinel"),
        root.join("seeded-violation/new"),
    ] {
        let error = if denied.exists() {
            fs::read(&denied).unwrap_err()
        } else {
            fs::write(&denied, b"must-not-land").unwrap_err()
        };
        assert_eq!(
            error.raw_os_error(),
            Some(libc::EACCES),
            "{}",
            denied.display()
        );
    }
    let error = fs::write(root.join("app/new"), b"must-not-land").unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::EACCES));
}

#[derive(Default)]
struct FakeKernel {
    abi: Option<u32>,
    applied: Vec<EffectiveRuleset>,
}

impl LandlockKernel for FakeKernel {
    fn abi(&mut self) -> io::Result<u32> {
        self.abi
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOSYS))
    }

    fn enforce(&mut self, ruleset: &EffectiveRuleset) -> io::Result<()> {
        self.applied.push(ruleset.clone());
        Ok(())
    }
}

#[test]
fn required_mode_refuses_missing_and_old_abi() {
    let root = PathBuf::from("/fixture");
    let required = policy(&root, true);

    let missing = apply_landlock(&required, &mut FakeKernel::default()).unwrap_err();
    assert_eq!(
        missing.to_string(),
        "Landlock unavailable: Function not implemented (os error 38)"
    );

    let mut old = FakeKernel {
        abi: Some(9),
        ..FakeKernel::default()
    };
    let error = apply_landlock(&required, &mut old).unwrap_err();
    assert_eq!(error.to_string(), "Landlock ABI 9 is below required ABI 10");
    assert!(old.applied.is_empty());

    let mut exact = FakeKernel {
        abi: Some(10),
        ..FakeKernel::default()
    };
    let applied = apply_landlock(&required, &mut exact).unwrap();
    assert_eq!(applied.kernel_abi, 10);
    assert_eq!(exact.applied.len(), 1);

    // Browser scopes are independently gated at ABI 6 and never acquire FS/net rules.
    let mut browser = required.clone();
    browser.class = AppClass::NestedUserns;
    browser.landlock.required_abi = 6;
    browser.landlock.filesystem.clear();
    browser.landlock.handles_network = false;
    browser.landlock.network.clear();
    let error = apply_landlock(
        &browser,
        &mut FakeKernel {
            abi: Some(5),
            ..FakeKernel::default()
        },
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "Landlock ABI 5 is below required ABI 6");
    let mut abi6 = FakeKernel {
        abi: Some(6),
        ..FakeKernel::default()
    };
    let browser_applied = apply_landlock(&browser, &mut abi6).unwrap();
    let browser_rules = browser_applied.ruleset.unwrap();
    assert!(browser_rules.filesystem.is_empty());
    assert!(browser_rules.network.is_empty());
    assert_ne!(browser_rules.scoped, 0);

    // A non-production BestEffort profile applies the ABI-4 subset, masking ABI-6 scopes and
    // ABI-10 UDP instead of claiming those rights were enforced.
    let mut best_effort = required.clone();
    best_effort.landlock.downgrade_mode = DowngradeMode::BestEffort;
    best_effort.landlock.network = vec![NetRule {
        access: NetAccess::ConnectSendUdp,
        port: 443,
    }];
    let mut abi4 = FakeKernel {
        abi: Some(4),
        ..FakeKernel::default()
    };
    let masked = apply_landlock(&best_effort, &mut abi4)
        .unwrap()
        .ruleset
        .unwrap();
    assert_eq!(masked.scoped, 0);
    assert!(masked.network.is_empty());
}
