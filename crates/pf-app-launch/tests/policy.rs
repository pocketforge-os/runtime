use pf_app_launch::policy::{
    derive_policy, render_systemd_dropin, AppClass, FsAccess, MediatedEndpoint, NetAccess,
    PolicyInputs, PolicyResourceInventory,
};
use pf_app_manifest::{parse_manifest, parse_platform_contract, ResolvedApp};
use std::collections::BTreeMap;
use std::io;
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
            let token = token.strip_suffix('?').unwrap_or(token);
            let base = token.split_once(':').map_or(token, |(base, _)| base);
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
            "broker-sockets".into(),
            vec![PathBuf::from("/run/pocketforge/input-broker.sock")],
        ),
        (
            "display-sockets".into(),
            vec![PathBuf::from("/run/pocketforge/session/wayland-0")],
        ),
        (
            "input-devices".into(),
            vec![PathBuf::from("/dev/input/event0")],
        ),
        (
            "video-decode-devices".into(),
            vec![PathBuf::from("/dev/video0")],
        ),
        (
            "render-devices".into(),
            vec![PathBuf::from("/dev/dri/renderD128")],
        ),
        ("media-devices".into(), vec![PathBuf::from("/dev/media0")]),
        (
            "session-environment".into(),
            vec![PathBuf::from("/run/pocketforge/session/environment")],
        ),
        (
            "x11-environment".into(),
            vec![PathBuf::from(
                "/run/pocketforge/session/capabilities/x11/environment",
            )],
        ),
        (
            "x11-socket".into(),
            vec![PathBuf::from(
                "/run/pocketforge/session/capabilities/x11/X0",
            )],
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
fn x11_projection_is_exact_manifest_scoped_and_fail_closed() {
    let root = PathBuf::from("/fixture");
    let socket = Path::new("/run/pocketforge/session/capabilities/x11/X0");
    let mapping = "BindPaths=/run/pocketforge/session/capabilities/x11/X0:/tmp/.X11-unix/X0";

    let x11 = derive_policy(&resolved(&root, &["x11"], false, true), &inputs(&root)).unwrap();
    let rendered = render_systemd_dropin(&x11);
    assert_eq!(rendered.lines().filter(|line| *line == mapping).count(), 1);
    assert!(x11
        .landlock
        .filesystem
        .iter()
        .any(|rule| { rule.path == socket && rule.access == FsAccess::UnixConnect }));
    assert!(!rendered.contains("@/tmp/.X11-unix/X0"));

    let plain = derive_policy(&resolved(&root, &[], false, true), &inputs(&root)).unwrap();
    let plain_rendered = render_systemd_dropin(&plain);
    assert!(!plain_rendered.contains("capabilities/x11"));
    assert!(!plain_rendered.contains("/tmp/.X11-unix/X0"));
    assert!(!serde_json::to_string(&plain)
        .unwrap()
        .contains("capabilities/x11"));
    assert!(!plain
        .landlock
        .filesystem
        .iter()
        .any(|rule| rule.path == socket));

    for missing in ["x11-socket", "x11-environment"] {
        let mut incomplete = inputs(&root);
        incomplete.resources.remove(missing);
        assert_eq!(
            derive_policy(&resolved(&root, &["x11"], false, true), &incomplete)
                .unwrap_err()
                .to_string(),
            format!("required x11 resource {missing} is missing")
        );
    }

    for (class, path) in [
        (
            "x11-socket",
            "/run/pocketforge/session/capabilities/x11/@X0",
        ),
        ("x11-environment", "/run/pocketforge/session/environment"),
    ] {
        let mut malformed = inputs(&root);
        malformed
            .resources
            .insert(class.into(), vec![PathBuf::from(path)]);
        assert!(
            derive_policy(&resolved(&root, &["x11"], false, true), &malformed)
                .unwrap_err()
                .to_string()
                .contains("canonical")
        );
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
fn device_regression_input_selects_broker_or_raw_route() {
    let root = PathBuf::from("/fixture");
    let app = resolved(&root, &["input"], false, true);
    let connect = |policy: &pf_app_launch::policy::PolicyDecision, path: &Path| {
        policy
            .landlock
            .filesystem
            .iter()
            .any(|rule| rule.path == path && rule.access == FsAccess::UnixConnect)
            .then_some(())
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EACCES))
    };

    // Build 10 inventories raw evdev while PF_BROKER_SOCK points at the broker. On the old
    // compiler this exact connect outcome was EACCES even though the raw route derived.
    let both = derive_policy(&app, &inputs(&root)).unwrap();
    connect(&both, Path::new("/run/pocketforge/input-broker.sock")).unwrap();
    assert!(both.endpoints.contains(&MediatedEndpoint::InputBroker));
    assert!(both
        .unit
        .device_allow
        .iter()
        .any(|grant| grant.path == Path::new("/dev/input/event0") && grant.mode == "r"));
    for denied in [
        "/run/pocketforge/prefsd.sock",
        "/run/pocketforge/session-authority.sock",
        "/run/pocketforge/system-menu.sock",
    ] {
        assert_eq!(
            connect(&both, Path::new(denied))
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EACCES)
        );
    }

    let mut broker_only = inputs(&root);
    broker_only.resources.remove("input-devices");
    let broker = derive_policy(&app, &broker_only).unwrap();
    assert!(broker.endpoints.contains(&MediatedEndpoint::InputBroker));
    assert!(broker.unit.device_allow.is_empty());
    assert!(!broker
        .unit
        .supplementary_groups
        .iter()
        .any(|group| group == "input"));
    assert!(broker
        .unit
        .bind_read_write
        .contains(&PathBuf::from("/run/pocketforge/input-broker.sock")));
    assert!(broker.landlock.filesystem.iter().any(|rule| {
        rule.path == Path::new("/run/pocketforge/input-broker.sock")
            && rule.access == FsAccess::UnixConnect
    }), "UnixConnect to /run/pocketforge/input-broker.sock under the derived ruleset would return EACCES");
    for denied in [
        "/run/pocketforge/prefsd.sock",
        "/run/pocketforge/session-authority.sock",
        "/run/pocketforge/system-menu.sock",
    ] {
        assert!(!broker.unit.bind_read_write.contains(&PathBuf::from(denied)));
        assert!(!broker
            .landlock
            .filesystem
            .iter()
            .any(|rule| rule.path == Path::new(denied)));
    }

    let mut raw_only = inputs(&root);
    raw_only.resources.remove("broker-sockets");
    let raw = derive_policy(&app, &raw_only).unwrap();
    assert!(!raw.endpoints.contains(&MediatedEndpoint::InputBroker));
    assert!(raw
        .unit
        .supplementary_groups
        .iter()
        .any(|group| group == "input"));
    assert!(raw
        .unit
        .device_allow
        .iter()
        .any(|grant| grant.path == Path::new("/dev/input/event0") && grant.mode == "r"));

    let mut no_route = inputs(&root);
    no_route.resources.remove("broker-sockets");
    no_route.resources.remove("input-devices");
    assert_eq!(
        derive_policy(&app, &no_route).unwrap_err().to_string(),
        "required input resource is missing: expected broker-sockets or input-devices"
    );
    derive_policy(&resolved(&root, &["input?"], false, true), &no_route).unwrap();
}

#[test]
fn input_actuators_share_routes_and_escalate_only_raw_devices() {
    let root = PathBuf::from("/fixture");
    let app = resolved(&root, &["input", "rumble", "vibration"], false, true);

    let mut broker_only = inputs(&root);
    broker_only.resources.remove("input-devices");
    let broker = derive_policy(&app, &broker_only).unwrap();
    assert!(broker.endpoints.contains(&MediatedEndpoint::InputBroker));
    assert!(broker.unit.device_allow.is_empty());

    let mut raw_only = inputs(&root);
    raw_only.resources.remove("broker-sockets");
    let raw = derive_policy(&app, &raw_only).unwrap();
    assert!(raw
        .unit
        .device_allow
        .iter()
        .any(|grant| grant.path == Path::new("/dev/input/event0") && grant.mode == "rw"));

    let mut no_route = inputs(&root);
    no_route.resources.remove("broker-sockets");
    no_route.resources.remove("input-devices");
    derive_policy(
        &resolved(&root, &["rumble?", "vibration?"], false, true),
        &no_route,
    )
    .unwrap();
    assert_eq!(
        derive_policy(
            &resolved(&root, &["input?", "rumble"], false, true),
            &no_route,
        )
        .unwrap_err()
        .to_string(),
        "required input resource is missing: expected broker-sockets or input-devices"
    );
}

#[test]
fn bluetooth_policy_remains_separate_from_input_routes() {
    let root = PathBuf::from("/fixture");
    let policy = derive_policy(
        &resolved(&root, &["bluetooth"], false, true),
        &inputs(&root),
    )
    .unwrap();

    assert!(policy.endpoints.contains(&MediatedEndpoint::BluetoothPort));
    assert!(!policy.endpoints.contains(&MediatedEndpoint::InputBroker));
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
fn required_audio_accepts_any_available_route_and_rejects_none() {
    let root = PathBuf::from("/fixture");
    let mut app = resolved(&root, &["audio"], false, true);
    let launch = app.manifest.launch.as_mut().unwrap();
    launch.audio = true;
    assert!(!launch.takes_display);

    // Poolsuite's direct-ALSA profile has only device resources. It must not
    // acquire a display-session environment or an unproduced audio socket.
    let mut devices_only = inputs(&root);
    devices_only.resources.remove("audio-sockets");
    devices_only.resources.remove("session-environment");
    let direct = derive_policy(&app, &devices_only).unwrap();
    assert!(direct.unit.bind_read_only.is_empty());
    assert!(!direct
        .unit
        .bind_read_write
        .contains(&PathBuf::from("/run/pocketforge/session/audio-0")));
    assert!(direct
        .unit
        .device_allow
        .iter()
        .any(|grant| grant.path == Path::new("/dev/snd/controlC0") && grant.mode == "rw"));

    // A future audio relay may satisfy the same required capability without
    // exposing ALSA devices directly.
    let mut socket_only = inputs(&root);
    socket_only.resources.remove("audio-devices");
    socket_only.resources.remove("session-environment");
    let relayed = derive_policy(&app, &socket_only).unwrap();
    assert!(relayed.unit.device_allow.is_empty());
    assert!(relayed
        .unit
        .bind_read_write
        .contains(&PathBuf::from("/run/pocketforge/session/audio-0")));

    // Required audio still fails closed when the image offers no audio route.
    let mut no_route = inputs(&root);
    no_route.resources.remove("audio-devices");
    no_route.resources.remove("audio-sockets");
    no_route.resources.remove("session-environment");
    let error = derive_policy(&app, &no_route).unwrap_err();
    assert_eq!(
        error.to_string(),
        "required audio resource is missing: expected audio-devices or audio-sockets"
    );

    // Optional audio preserves its absence-tolerant behavior.
    let optional = resolved(&root, &["audio?"], false, true);
    derive_policy(&optional, &no_route).unwrap();
}

#[test]
fn required_display_accepts_complete_direct_or_session_routes_and_rejects_neither() {
    let root = PathBuf::from("/fixture");
    let mut app = resolved(&root, &["audio"], false, true);
    let launch = app.manifest.launch.as_mut().unwrap();
    launch.takes_display = true;
    launch.audio = true;

    // Poolsuite on a133-open uses direct framebuffer display and direct ALSA.
    let mut direct_only = inputs(&root);
    direct_only.resources.remove("audio-sockets");
    direct_only.resources.remove("display-sockets");
    direct_only.resources.remove("session-environment");
    direct_only
        .resources
        .insert("display-devices".into(), vec![PathBuf::from("/dev/fb0")]);
    let direct = derive_policy(&app, &direct_only).unwrap();
    assert!(direct
        .unit
        .device_allow
        .iter()
        .any(|grant| grant.path == Path::new("/dev/fb0") && grant.mode == "rw"));
    assert!(direct.landlock.filesystem.iter().any(|rule| {
        rule.path == Path::new("/dev/fb0") && rule.access == FsAccess::DeviceReadWrite
    }));
    assert!(direct
        .unit
        .bind_read_write
        .contains(&PathBuf::from("/dev/fb0")));
    assert!(!direct
        .unit
        .bind_read_write
        .contains(&PathBuf::from("/run/pocketforge/session/wayland-0")));
    assert!(!direct
        .unit
        .bind_read_only
        .contains(&PathBuf::from("/run/pocketforge/session/environment")));
    let rendered_direct = render_systemd_dropin(&direct);
    assert!(rendered_direct.contains("BindPaths=/dev/fb0"));
    assert!(rendered_direct.contains("DeviceAllow=/dev/fb0 rw"));

    // A compositor session is independently a complete display route.
    let session_only = inputs(&root);
    let session = derive_policy(&app, &session_only).unwrap();
    assert!(session
        .unit
        .bind_read_write
        .contains(&PathBuf::from("/run/pocketforge/session/wayland-0")));
    assert!(session
        .unit
        .bind_read_only
        .contains(&PathBuf::from("/run/pocketforge/session/environment")));
    assert!(!session
        .unit
        .device_allow
        .iter()
        .any(|grant| grant.path == Path::new("/dev/fb0")));

    // If the image intentionally offers both complete routes, preserve both.
    let mut both_routes = inputs(&root);
    both_routes
        .resources
        .insert("display-devices".into(), vec![PathBuf::from("/dev/fb0")]);
    let both = derive_policy(&app, &both_routes).unwrap();
    assert!(both
        .unit
        .device_allow
        .iter()
        .any(|grant| grant.path == Path::new("/dev/fb0") && grant.mode == "rw"));
    assert!(both
        .unit
        .bind_read_write
        .contains(&PathBuf::from("/run/pocketforge/session/wayland-0")));
    assert!(both
        .unit
        .bind_read_only
        .contains(&PathBuf::from("/run/pocketforge/session/environment")));

    let expected = "required display resource is missing: expected display-devices or both \
                    display-sockets and session-environment";
    let mut no_route = inputs(&root);
    no_route.resources.remove("display-sockets");
    no_route.resources.remove("session-environment");
    assert_eq!(
        derive_policy(&app, &no_route).unwrap_err().to_string(),
        expected
    );

    let mut socket_only = inputs(&root);
    socket_only.resources.remove("session-environment");
    assert_eq!(
        derive_policy(&app, &socket_only).unwrap_err().to_string(),
        expected
    );

    let mut environment_only = inputs(&root);
    environment_only.resources.remove("display-sockets");
    assert_eq!(
        derive_policy(&app, &environment_only)
            .unwrap_err()
            .to_string(),
        expected
    );

    // A complete direct route remains valid when an incomplete session route is inventoried,
    // but the incomplete route is not granted. This covers the remaining two combinations of
    // the three route-resource axes.
    for missing_session_class in ["display-sockets", "session-environment"] {
        let mut direct_with_partial_session = inputs(&root);
        direct_with_partial_session
            .resources
            .remove(missing_session_class);
        direct_with_partial_session
            .resources
            .insert("display-devices".into(), vec![PathBuf::from("/dev/fb0")]);
        let policy = derive_policy(&app, &direct_with_partial_session).unwrap();
        assert!(policy
            .unit
            .device_allow
            .iter()
            .any(|grant| grant.path == Path::new("/dev/fb0") && grant.mode == "rw"));
        assert!(!policy
            .unit
            .bind_read_write
            .contains(&PathBuf::from("/run/pocketforge/session/wayland-0")));
        assert!(!policy
            .unit
            .bind_read_only
            .contains(&PathBuf::from("/run/pocketforge/session/environment")));
    }

    let mut missing_render = direct_only;
    missing_render.resources.remove("render-devices");
    assert_eq!(
        derive_policy(&app, &missing_render)
            .unwrap_err()
            .to_string(),
        "required resource render-devices is missing"
    );
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

    // Negative compatibility control: the manifest parser preserves legacy
    // schema-1 host-only egress, but the one policy compiler cannot turn it
    // into a protocol/port Landlock decision and therefore fails closed.
    let error = derive_policy(
        &resolved(&root, &["egress:scores.example.com"], true, true),
        &inputs(&root),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "egress authority is invalid");
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
