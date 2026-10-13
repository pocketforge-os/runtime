use pf_app_launch::policy::{
    derive_policy, render_systemd_dropin, AppClass, MediatedEndpoint, NetAccess, PolicyInputs,
    PolicyResourceInventory,
};
use pf_app_manifest::{parse_manifest, parse_platform_contract, ResolvedApp};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn resolved(
    root: &Path,
    capabilities: &[&str],
    needs_network: bool,
    production: bool,
) -> ResolvedApp {
    let capability_source = capabilities
        .iter()
        .map(|capability| format!("{capability:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let manifest = parse_manifest(&format!(
        "[app]\nid = \"org.example.app\"\nuse = [{capability_source}]\n\
         [runtime]\nfamily = \"pocketforge/a133-powervr\"\nabi = \"1\"\n\
         [launch]\nexec = \"bin/app\"\nneeds_network = {needs_network}\n"
    ))
    .unwrap();
    let mode = if production {
        "required"
    } else {
        "best_effort"
    };
    let supported = capabilities
        .iter()
        .filter_map(|token| {
            let base = token.split_once(':').map_or(*token, |(base, _)| base);
            (base != "egress").then_some(format!("{base:?}"))
        })
        .collect::<Vec<_>>()
        .join(", ");
    let platform = parse_platform_contract(&format!(
        "schema_version = 1\nruntime_family = \"pocketforge/a133-powervr\"\n\
         runtime_abi = \"1\"\nplatform_version = \"20\"\n\
         supported_capabilities = [{supported}]\nproduction = {production}\n\
         landlock_mode = \"{mode}\"\nlandlock_full_min_abi = 10\n\
         landlock_browser_min_abi = 6\n"
    ))
    .unwrap();

    ResolvedApp {
        id: "org.example.app".into(),
        app_dir: root.join("app"),
        executable: root.join("app/bin/app"),
        manifest,
        manifest_sha256: "fixture-manifest".into(),
        platform,
    }
}

fn inputs(root: &Path) -> PolicyInputs {
    let resources = BTreeMap::from([
        (
            "audio-devices".into(),
            vec![PathBuf::from("/dev/snd/controlC0")],
        ),
        (
            "audio-sockets".into(),
            vec![PathBuf::from("/run/pocketforge/session/audio-0")],
        ),
        (
            "input-devices".into(),
            vec![PathBuf::from("/dev/input/event0")],
        ),
        (
            "video-decode-devices".into(),
            vec![PathBuf::from("/dev/video0")],
        ),
        ("media-devices".into(), vec![PathBuf::from("/dev/media0")]),
        (
            "session-environment".into(),
            vec![PathBuf::from("/run/pocketforge/session/environment")],
        ),
    ]);
    PolicyInputs {
        platform_runtime: root.join("platform-runtime"),
        xdg_config: root.join("xdg/config/org.example.app"),
        xdg_state: root.join("xdg/state/org.example.app"),
        xdg_cache: root.join("xdg/cache/org.example.app"),
        resource_paths: vec![root.join("platform-dependencies")],
        resources,
    }
}

#[test]
fn browser_profile_has_scopes_and_no_fs_or_net_rules() {
    let root = PathBuf::from("/fixture");
    let browser = derive_policy(
        &resolved(
            &root,
            &["sandbox.nested_userns", "egress:tcp/443@example.com"],
            true,
            true,
        ),
        &inputs(&root),
    )
    .unwrap();
    let full = derive_policy(&resolved(&root, &[], false, true), &inputs(&root)).unwrap();

    assert_eq!(browser.class, AppClass::NestedUserns);
    assert!(browser.landlock.filesystem.is_empty());
    assert!(browser.landlock.network.is_empty());
    assert!(browser.network.dns);
    assert!(browser.landlock.scope_signal);
    assert!(browser.landlock.scope_abstract_unix);
    assert_eq!(browser.landlock.required_abi, 6);

    assert_eq!(full.class, AppClass::Full);
    assert!(!full.landlock.filesystem.is_empty());
    assert!(full.landlock.handles_network);
    assert_eq!(full.landlock.required_abi, 10);
}

#[test]
fn input_and_bluetooth_capabilities_are_mediated_not_raw() {
    let root = PathBuf::from("/fixture");
    let policy = derive_policy(
        &resolved(&root, &["bluetooth", "input"], false, true),
        &inputs(&root),
    )
    .unwrap();

    // C3 keeps input's current raw grant until tsp-mc9m.41.996.9 flips this one compiler.
    assert!(policy
        .unit
        .supplementary_groups
        .iter()
        .any(|group| group == "input"));
    assert!(policy
        .unit
        .device_allow
        .iter()
        .any(|grant| grant.path == Path::new("/dev/input/event0") && grant.mode == "r"));
    assert!(!policy.endpoints.contains(&MediatedEndpoint::InputBroker));

    assert!(policy.endpoints.contains(&MediatedEndpoint::BluetoothPort));
    assert!(!policy
        .unit
        .restrict_address_families
        .iter()
        .any(|family| family == "AF_BLUETOOTH"));
    assert!(policy
        .unit
        .inaccessible_paths
        .contains(&PathBuf::from("/run/dbus/system_bus_socket")));
}

#[test]
fn headless_video_decode_gets_the_device_owning_group() {
    let root = PathBuf::from("/fixture");
    let app = resolved(&root, &["video-decode"], false, true);
    assert!(!app.manifest.launch.as_ref().unwrap().takes_display);
    let policy = derive_policy(&app, &inputs(&root)).unwrap();

    assert!(policy
        .unit
        .supplementary_groups
        .iter()
        .any(|group| group == "video"));
    assert!(policy
        .unit
        .device_allow
        .iter()
        .any(|grant| grant.path == Path::new("/dev/video0") && grant.mode == "rw"));
    assert!(policy
        .unit
        .device_allow
        .iter()
        .any(|grant| grant.path == Path::new("/dev/media0") && grant.mode == "rw"));
}

#[test]
fn rendered_unit_and_runtime_policy_share_one_decision() {
    let root = PathBuf::from("/fixture");
    let policy = derive_policy(
        &resolved(
            &root,
            &["audio", "egress:tcp/443@example.com", "input"],
            true,
            true,
        ),
        &inputs(&root),
    )
    .unwrap();
    let rendered = render_systemd_dropin(&policy);

    assert!(policy.landlock.tcp_connect_ports.contains(&443));
    assert!(policy.landlock.filesystem.iter().any(|rule| {
        rule.path == root.join("platform-dependencies")
            && rule.access == pf_app_launch::policy::FsAccess::ReadExecute
    }));
    assert!(policy
        .network
        .authorities
        .iter()
        .any(|authority| authority.host == "example.com" && authority.port == 443));
    assert!(rendered.contains("SupplementaryGroups=audio input"));
    assert!(rendered.contains("RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6"));
    assert!(rendered.contains("# egress tcp example.com 443"));
    assert!(!rendered.contains("AF_BLUETOOTH"));

    let mut invalid_inputs = inputs(&root);
    invalid_inputs
        .resources
        .insert("input-devices".into(), vec![PathBuf::from("../event0")]);
    let error =
        derive_policy(&resolved(&root, &["input"], false, true), &invalid_inputs).unwrap_err();
    assert!(error
        .to_string()
        .contains("policy input path is not a confined absolute path"));
}

#[test]
fn networked_policy_derives_dns_without_manifest_capability() {
    let root = PathBuf::from("/fixture");
    let networked = derive_policy(
        &resolved(&root, &["egress:tcp/80@*", "egress:tcp/443@*"], true, true),
        &inputs(&root),
    )
    .unwrap();
    let offline = derive_policy(&resolved(&root, &[], false, true), &inputs(&root)).unwrap();

    assert!(networked.network.dns);
    assert_eq!(networked.landlock.tcp_connect_ports, vec![53, 80, 443]);
    assert_eq!(networked.landlock.udp_connect_ports, vec![53]);
    assert!(networked
        .landlock
        .network
        .contains(&pf_app_launch::policy::NetRule {
            access: NetAccess::BindUdpEphemeral,
            port: 0,
        }));
    assert!(render_systemd_dropin(&networked).contains("# derived dns tcp udp 53"));

    assert!(!offline.network.dns);
    assert!(offline.landlock.tcp_connect_ports.is_empty());
    assert!(offline.landlock.udp_connect_ports.is_empty());
}

#[test]
fn resource_inventory_requires_its_platform_runtime() {
    let missing = serde_json::from_str::<PolicyResourceInventory>(r#"{"resources":{}}"#);
    assert!(missing.is_err());

    let inventory: PolicyResourceInventory = serde_json::from_str(
        r#"{"platform_runtime":"/usr","resources":{"input-devices":["/dev/input/event0"]}}"#,
    )
    .unwrap();
    assert_eq!(inventory.platform_runtime, Path::new("/usr"));
    assert_eq!(
        inventory.resources["input-devices"],
        vec![PathBuf::from("/dev/input/event0")]
    );
}
