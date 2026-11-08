//! The single manifest-to-enforcement policy compiler used by the launcher and image renderer.

use pf_app_manifest::{
    parse_capability_requirement, parse_egress_authority, EgressAuthority, EgressProtocol,
    LandlockMode, ResolvedApp,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

pub const POLICY_SCHEMA_VERSION: u32 = 1;
const DNS_PORT: u16 = 53;
const X11_BIND: (&str, &str) = ("/run/pocketforge/x11/.X11-unix/X0", "/tmp/.X11-unix/X0");
pub const X11_SOCKET: &str = X11_BIND.0;
pub const X11_SOCKET_DESTINATION: &str = X11_BIND.1;
pub const X11_ENVIRONMENT: &str = "/run/pocketforge/x11/policy/environment";
pub const INPUT_BROKER_SOCKET: &str = "/run/pocketforge/input-broker.sock";
const APP_DATA_ROOT: &str = "/var/lib/pocketforge/apps";

#[derive(Clone, Copy)]
enum DisplayResourceGrant {
    ReadOnly,
    Socket,
    DeviceReadWrite,
}

#[derive(Clone, Copy)]
struct DisplayRouteResource {
    class: &'static str,
    grant: DisplayResourceGrant,
}

struct DisplayRoute {
    requirement: &'static str,
    resources: &'static [DisplayRouteResource],
}

// A display route is complete only when every resource class in one row is present. Future
// variants (for example an explicitly mediated X11 route) extend this table instead of adding a
// parallel display-policy branch.
const DISPLAY_ROUTES: &[DisplayRoute] = &[
    DisplayRoute {
        requirement: "display-devices",
        resources: &[DisplayRouteResource {
            class: "display-devices",
            grant: DisplayResourceGrant::DeviceReadWrite,
        }],
    },
    DisplayRoute {
        requirement: "both display-sockets and session-environment",
        resources: &[
            DisplayRouteResource {
                class: "display-sockets",
                grant: DisplayResourceGrant::Socket,
            },
            DisplayRouteResource {
                class: "session-environment",
                grant: DisplayResourceGrant::ReadOnly,
            },
        ],
    },
];

#[derive(Clone, Copy)]
enum InputRouteGrant {
    BrokerSocket,
    RawDevice,
}

struct InputRoute {
    requirement: &'static str,
    class: &'static str,
    grant: InputRouteGrant,
}

// The broker and raw-device routes are inventory-selected. The shipped input broker uses the
// same PFW1 socket for input acquisition, rumble and sensor operations, so its existing
// broker-sockets class is deliberately shared instead of aliasing one path under two class names.
const INPUT_ROUTES: &[InputRoute] = &[
    InputRoute {
        requirement: "broker-sockets",
        class: "broker-sockets",
        grant: InputRouteGrant::BrokerSocket,
    },
    InputRoute {
        requirement: "input-devices",
        class: "input-devices",
        grant: InputRouteGrant::RawDevice,
    },
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyInputs {
    pub platform_runtime: PathBuf,
    pub xdg_config: PathBuf,
    pub xdg_state: PathBuf,
    pub xdg_cache: PathBuf,
    pub resource_paths: Vec<PathBuf>,
    pub resources: BTreeMap<String, Vec<PathBuf>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PolicyResourceInventory {
    pub platform_runtime: PathBuf,
    #[serde(default)]
    pub resources: BTreeMap<String, Vec<PathBuf>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppClass {
    Full,
    NestedUserns,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DowngradeMode {
    Required,
    BestEffort,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsAccess {
    ReadExecute,
    ReadOnly,
    ReadWriteNoExec,
    DeviceReadOnly,
    DeviceReadWrite,
    UnixConnect,
    /// `LANDLOCK_ACCESS_FS_READ_DIR` only: enumerate a directory, open nothing in it.
    ListDirectory,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FsRule {
    pub path: PathBuf,
    pub access: FsAccess,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetAccess {
    ConnectTcp,
    BindUdpEphemeral,
    ConnectSendUdp,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NetRule {
    pub access: NetAccess,
    pub port: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LandlockDecision {
    pub required_abi: u32,
    pub downgrade_mode: DowngradeMode,
    pub filesystem: Vec<FsRule>,
    pub handles_network: bool,
    pub network: Vec<NetRule>,
    pub tcp_connect_ports: Vec<u16>,
    pub udp_connect_ports: Vec<u16>,
    pub scope_signal: bool,
    pub scope_abstract_unix: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NetworkDecision {
    pub authorities: Vec<EgressAuthority>,
    /// Resolver transport derived for every networked decision, never app-declared.
    pub dns: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediatedEndpoint {
    InputBroker,
    BluetoothPort,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeviceGrant {
    pub path: PathBuf,
    pub mode: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BindMapping {
    pub source: PathBuf,
    pub destination: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentProjection {
    pub source: PathBuf,
    pub name: String,
    pub expected_value: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UnitProperties {
    pub supplementary_groups: Vec<String>,
    pub restrict_namespaces: Vec<String>,
    pub restrict_address_families: Vec<String>,
    pub private_network: bool,
    pub inaccessible_paths: Vec<PathBuf>,
    /// Fetch destinations made read-only in the mount namespace. Each entry
    /// renders as `ReadOnlyPaths=-<path>`; absent when the app declares no
    /// `[fetch] destination = "var"`, keeping that policy byte-identical.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_only_paths: Vec<PathBuf>,
    pub bind_read_only: Vec<PathBuf>,
    pub bind_read_write: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bind_read_write_mappings: Vec<BindMapping>,
    pub device_allow: Vec<DeviceGrant>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PolicyDecision {
    pub schema_version: u32,
    pub app_id: String,
    pub manifest_sha256: String,
    pub class: AppClass,
    pub landlock: LandlockDecision,
    pub network: NetworkDecision,
    pub endpoints: Vec<MediatedEndpoint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub environment: Vec<EnvironmentProjection>,
    pub unit: UnitProperties,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyError(pub String);

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PolicyError {}

pub fn derive_policy(
    app: &ResolvedApp,
    inputs: &PolicyInputs,
) -> Result<PolicyDecision, PolicyError> {
    let all_input_paths = [
        &inputs.platform_runtime,
        &inputs.xdg_config,
        &inputs.xdg_state,
        &inputs.xdg_cache,
    ]
    .into_iter()
    .chain(inputs.resource_paths.iter())
    .chain(inputs.resources.values().flatten());
    for path in all_input_paths {
        if !confined_absolute(path) {
            return Err(PolicyError(format!(
                "policy input path is not a confined absolute path: {}",
                path.display()
            )));
        }
    }
    let mode = app
        .platform
        .landlock_mode
        .ok_or_else(|| PolicyError("signed platform contract has no Landlock policy".to_owned()))?;
    let downgrade_mode = match mode {
        LandlockMode::Required => DowngradeMode::Required,
        LandlockMode::BestEffort if !app.platform.production => DowngradeMode::BestEffort,
        LandlockMode::BestEffort => {
            return Err(PolicyError(
                "production contract cannot use best_effort Landlock".to_owned(),
            ));
        }
    };

    let mut capability_bases = BTreeSet::new();
    let mut capability_optional = BTreeMap::new();
    let mut authorities = BTreeSet::new();
    for token in &app.manifest.app.capabilities {
        let requirement = parse_capability_requirement(token);
        capability_bases.insert(requirement.base.clone());
        capability_optional.insert(requirement.base.clone(), requirement.optional);
        if requirement.base == "egress" {
            let authority = requirement
                .modifier
                .as_deref()
                .ok_or_else(|| PolicyError("egress authority is missing".to_owned()))
                .and_then(|modifier| {
                    parse_egress_authority(modifier)
                        .map_err(|_| PolicyError("egress authority is invalid".to_owned()))
                })?;
            authorities.insert(authority);
        }
    }
    let launch = app
        .manifest
        .launch
        .as_ref()
        .ok_or_else(|| PolicyError("launch table is missing".to_owned()))?;
    if launch.needs_network != !authorities.is_empty() {
        return Err(PolicyError(
            "launch.needs_network must match explicit egress authorities".to_owned(),
        ));
    }

    let class = if capability_bases.contains("sandbox.nested_userns") {
        AppClass::NestedUserns
    } else {
        AppClass::Full
    };
    let required_abi = match class {
        AppClass::Full => app.platform.landlock_full_min_abi,
        AppClass::NestedUserns => app.platform.landlock_browser_min_abi,
    }
    .ok_or_else(|| {
        PolicyError("signed platform contract has no Landlock ABI minimum".to_owned())
    })?;

    // Fetch payloads are installed by the same uid as the app, so the sandbox
    // must take the destination away rather than rely on DAC. Resolve the
    // declared per-file dest exactly as the fetcher does: one component under
    // the app's state root. Only `destination = "var"` lands there; `home` and
    // an absent destination keep today's behaviour.
    let state_root = PathBuf::from(APP_DATA_ROOT).join(&app.id);
    let mut fetch_enabled = false;
    let mut fetch_destinations = BTreeSet::new();
    if let Some(fetch) = app.manifest.fetch.as_ref().filter(|fetch| fetch.enabled) {
        fetch_enabled = true;
        if fetch.destination.as_deref() == Some("var") {
            for file in fetch.files.as_deref().unwrap_or_default() {
                fetch_destinations.insert(fetch_destination(&state_root, file.dest.as_deref())?);
            }
        }
    }

    let mut filesystem = Vec::new();
    if class == AppClass::Full {
        filesystem.push(FsRule {
            path: app.app_dir.clone(),
            access: FsAccess::ReadExecute,
        });
        filesystem.push(FsRule {
            path: inputs.platform_runtime.clone(),
            access: FsAccess::ReadExecute,
        });
        for path in [&inputs.xdg_config, &inputs.xdg_state, &inputs.xdg_cache] {
            filesystem.push(FsRule {
                path: path.clone(),
                access: FsAccess::ReadWriteNoExec,
            });
        }
        filesystem.push(FsRule {
            path: PathBuf::from("/run/pf-app").join(&app.id),
            access: FsAccess::ReadWriteNoExec,
        });
        for path in &inputs.resource_paths {
            filesystem.push(FsRule {
                path: path.clone(),
                access: FsAccess::ReadExecute,
            });
        }
        // Fetched payloads are data, never executables: read file and read dir
        // only, with no write, execute, make-*, remove or truncate rights.
        for path in &fetch_destinations {
            filesystem.push(FsRule {
                path: path.clone(),
                access: FsAccess::ReadOnly,
            });
        }
    }

    let mut read_only_paths = BTreeSet::new();
    let mut socket_paths = BTreeSet::new();
    let mut landlock_only_socket_paths = BTreeSet::new();
    let mut bind_read_write_mappings = Vec::new();
    let mut environment = Vec::new();
    let mut devices: BTreeMap<PathBuf, String> = BTreeMap::new();
    // Raw input identity sources the app sees under a different /dev/input/eventN name.
    let mut device_destinations: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    let mut landlock_only_listings: BTreeSet<PathBuf> = BTreeSet::new();
    let mut groups = BTreeSet::new();
    let mut uses_input_broker = false;
    let resource = |name: &str, optional: bool| -> Result<Vec<PathBuf>, PolicyError> {
        match inputs.resources.get(name) {
            Some(paths) if !paths.is_empty() => Ok(paths.clone()),
            _ if optional => Ok(Vec::new()),
            _ => Err(PolicyError(format!("required resource {name} is missing"))),
        }
    };
    let add_devices = |devices: &mut BTreeMap<PathBuf, String>,
                       name: &str,
                       mode: &str,
                       optional: bool|
     -> Result<(), PolicyError> {
        for path in resource(name, optional)? {
            devices
                .entry(path)
                .and_modify(|current| {
                    if mode == "rw" {
                        *current = "rw".to_owned();
                    }
                })
                .or_insert_with(|| mode.to_owned());
        }
        Ok(())
    };

    if capability_bases.contains("x11") {
        // The generated drop-in owns the complete SupplementaryGroups value,
        // so preserve the group that gates the compositor-owned X11 socket.
        groups.insert("pf-session".to_owned());
        for (name, expected) in [
            ("x11-socket", X11_SOCKET),
            ("x11-environment", X11_ENVIRONMENT),
        ] {
            match inputs.resources.get(name) {
                Some(paths) if paths == &[PathBuf::from(expected)] => {}
                Some(_) => {
                    return Err(PolicyError(format!(
                        "required x11 resource {name} is not the canonical path {expected}"
                    )))
                }
                None => {
                    return Err(PolicyError(format!(
                        "required x11 resource {name} is missing"
                    )))
                }
            }
        }
        let socket = PathBuf::from(X11_BIND.0);
        landlock_only_socket_paths.insert(PathBuf::from(X11_SOCKET_DESTINATION));
        read_only_paths.insert(PathBuf::from(X11_ENVIRONMENT));
        bind_read_write_mappings.push(BindMapping {
            source: socket,
            destination: PathBuf::from(X11_BIND.1),
        });
        environment.push(EnvironmentProjection {
            source: PathBuf::from(X11_ENVIRONMENT),
            name: "DISPLAY".to_owned(),
            expected_value: ":0".to_owned(),
        });
    }

    if launch.takes_display {
        groups.insert("video".to_owned());
        let complete_routes = DISPLAY_ROUTES
            .iter()
            .filter(|route| {
                route.resources.iter().all(|required| {
                    inputs
                        .resources
                        .get(required.class)
                        .is_some_and(|paths| !paths.is_empty())
                })
            })
            .collect::<Vec<_>>();
        if complete_routes.is_empty() {
            let expected = DISPLAY_ROUTES
                .iter()
                .map(|route| route.requirement)
                .collect::<Vec<_>>()
                .join(" or ");
            return Err(PolicyError(format!(
                "required display resource is missing: expected {expected}"
            )));
        }
        for route in complete_routes {
            for required in route.resources {
                let paths = resource(required.class, false)?;
                match required.grant {
                    DisplayResourceGrant::ReadOnly => read_only_paths.extend(paths),
                    DisplayResourceGrant::Socket => socket_paths.extend(paths),
                    DisplayResourceGrant::DeviceReadWrite => {
                        for path in paths {
                            devices.insert(path, "rw".to_owned());
                        }
                    }
                }
            }
        }
        add_devices(&mut devices, "render-devices", "rw", false)?;
    }
    if launch.audio || capability_bases.contains("audio") {
        groups.insert("audio".to_owned());
        let optional = !launch.audio && capability_optional.get("audio") == Some(&true);
        let audio_sockets = resource("audio-sockets", true)?;
        let audio_devices = resource("audio-devices", true)?;
        if !optional && audio_sockets.is_empty() && audio_devices.is_empty() {
            return Err(PolicyError(
                "required audio resource is missing: expected audio-devices or audio-sockets"
                    .to_owned(),
            ));
        }
        socket_paths.extend(audio_sockets);
        for path in audio_devices {
            devices.insert(path, "rw".to_owned());
        }
    }
    let requested_input = ["input", "rumble", "vibration"]
        .into_iter()
        .filter(|name| capability_bases.contains(*name))
        .collect::<Vec<_>>();
    if !requested_input.is_empty() {
        let optional = requested_input
            .iter()
            .all(|name| capability_optional.get(*name) == Some(&true));
        let complete_routes = INPUT_ROUTES
            .iter()
            .filter(|route| {
                inputs
                    .resources
                    .get(route.class)
                    .is_some_and(|paths| !paths.is_empty())
            })
            .collect::<Vec<_>>();
        if complete_routes.is_empty() && !optional {
            let expected = INPUT_ROUTES
                .iter()
                .map(|route| route.requirement)
                .collect::<Vec<_>>()
                .join(" or ");
            return Err(PolicyError(format!(
                "required input resource is missing: expected {expected}"
            )));
        }
        let raw_mode = if requested_input
            .iter()
            .any(|name| matches!(*name, "rumble" | "vibration"))
        {
            "rw"
        } else {
            "r"
        };
        for route in complete_routes {
            match route.grant {
                InputRouteGrant::BrokerSocket => {
                    socket_paths.extend(resource(route.class, false)?);
                    uses_input_broker = true;
                }
                InputRouteGrant::RawDevice => {
                    groups.insert("input".to_owned());
                    add_devices(&mut devices, route.class, raw_mode, false)?;
                    let mut sources = resource(route.class, false)?;
                    sources.sort();
                    sources.dedup();
                    let mut taken = sources
                        .iter()
                        .filter(|path| is_event_node(path))
                        .cloned()
                        .collect::<BTreeSet<_>>();
                    let mut next = 0u32;
                    for source in sources.iter().filter(|path| !is_event_node(path)) {
                        let destination = loop {
                            let candidate = PathBuf::from(format!("/dev/input/event{next}"));
                            next += 1;
                            if taken.insert(candidate.clone()) {
                                break candidate;
                            }
                        };
                        device_destinations.insert(source.clone(), destination.clone());
                        bind_read_write_mappings.push(BindMapping {
                            source: source.clone(),
                            destination,
                        });
                    }
                    landlock_only_listings.insert(PathBuf::from(INPUT_DEVICE_DIRECTORY));
                }
            }
        }
    }
    if capability_bases.contains("video-decode") {
        let optional = capability_optional.get("video-decode") == Some(&true);
        add_devices(&mut devices, "video-decode-devices", "rw", optional)?;
        add_devices(&mut devices, "media-devices", "rw", optional)?;
        if ["video-decode-devices", "media-devices"]
            .into_iter()
            .any(|name| {
                inputs
                    .resources
                    .get(name)
                    .is_some_and(|paths| !paths.is_empty())
            })
        {
            groups.insert("video".to_owned());
        }
    }
    let brokered = [
        "accelerometer",
        "gnss",
        "gyroscope",
        "imu",
        "leds",
        "location",
        "magnetometer",
    ];
    let requested_brokered = brokered
        .into_iter()
        .filter(|name| capability_bases.contains(*name))
        .collect::<Vec<_>>();
    if !requested_brokered.is_empty() {
        let optional = requested_brokered
            .iter()
            .all(|name| capability_optional.get(*name) == Some(&true));
        let broker_paths = resource("broker-sockets", optional)?;
        if broker_paths.is_empty() {
            return Err(PolicyError(
                "optional broker-only capability requires broker-sockets to prevent direct backend fallback"
                    .to_owned(),
            ));
        }
        uses_input_broker = true;
        socket_paths.extend(broker_paths);
    }
    if class == AppClass::Full {
        filesystem.extend(read_only_paths.iter().cloned().map(|path| FsRule {
            path,
            access: FsAccess::ReadOnly,
        }));
        filesystem.extend(socket_paths.iter().cloned().map(|path| FsRule {
            path,
            access: FsAccess::UnixConnect,
        }));
        filesystem.extend(
            landlock_only_socket_paths
                .iter()
                .cloned()
                .map(|path| FsRule {
                    path,
                    access: FsAccess::UnixConnect,
                }),
        );
        filesystem.extend(landlock_only_listings.iter().cloned().map(|path| FsRule {
            path,
            access: FsAccess::ListDirectory,
        }));
        filesystem.extend(devices.iter().map(|(path, mode)| {
            FsRule {
                path: device_destinations
                    .get(path)
                    .cloned()
                    .unwrap_or_else(|| path.clone()),
                access: if mode == "rw" {
                    FsAccess::DeviceReadWrite
                } else {
                    FsAccess::DeviceReadOnly
                },
            }
        }));
    }
    for (path, mode) in &devices {
        if device_destinations.contains_key(path) {
            continue; // bound through its source:destination mapping
        }
        if mode == "rw" {
            socket_paths.insert(path.clone());
        } else {
            read_only_paths.insert(path.clone());
        }
    }

    let mut network_rules = Vec::new();
    let mut tcp_connect_ports = BTreeSet::new();
    let mut udp_connect_ports = BTreeSet::new();
    let networked = !authorities.is_empty();
    if class == AppClass::Full {
        if networked {
            tcp_connect_ports.insert(DNS_PORT);
            udp_connect_ports.insert(DNS_PORT);
            network_rules.push(NetRule {
                access: NetAccess::ConnectTcp,
                port: DNS_PORT,
            });
            network_rules.push(NetRule {
                access: NetAccess::ConnectSendUdp,
                port: DNS_PORT,
            });
        }
        for authority in &authorities {
            match authority.protocol {
                EgressProtocol::Tcp => {
                    if tcp_connect_ports.insert(authority.port) {
                        network_rules.push(NetRule {
                            access: NetAccess::ConnectTcp,
                            port: authority.port,
                        });
                    }
                }
                EgressProtocol::Udp => {
                    if udp_connect_ports.insert(authority.port) {
                        network_rules.push(NetRule {
                            access: NetAccess::ConnectSendUdp,
                            port: authority.port,
                        });
                    }
                }
            }
        }
        if !udp_connect_ports.is_empty() {
            network_rules.push(NetRule {
                access: NetAccess::BindUdpEphemeral,
                port: 0,
            });
        }
    }

    let supplementary_groups = groups.into_iter().collect();
    let device_allow = devices
        .into_iter()
        .map(|(path, mode)| DeviceGrant { path, mode })
        .collect();

    let mut endpoints = Vec::new();
    if uses_input_broker {
        endpoints.push(MediatedEndpoint::InputBroker);
    }
    if capability_bases.contains("bluetooth") {
        endpoints.push(MediatedEndpoint::BluetoothPort);
    }
    let mut address_families = vec!["AF_UNIX".to_owned()];
    if networked {
        address_families.extend(["AF_INET".to_owned(), "AF_INET6".to_owned()]);
    }
    // glibc getifaddrs/if_nametoindex (LAN discovery) use AF_NETLINK, so networked apps need it.
    if networked || class == AppClass::NestedUserns {
        address_families.push("AF_NETLINK".to_owned());
    }

    Ok(PolicyDecision {
        schema_version: POLICY_SCHEMA_VERSION,
        app_id: app.id.clone(),
        manifest_sha256: app.manifest_sha256.clone(),
        class,
        landlock: LandlockDecision {
            required_abi,
            downgrade_mode,
            filesystem,
            handles_network: class == AppClass::Full,
            network: network_rules,
            tcp_connect_ports: tcp_connect_ports.into_iter().collect(),
            udp_connect_ports: udp_connect_ports.into_iter().collect(),
            scope_signal: true,
            scope_abstract_unix: true,
        },
        network: NetworkDecision {
            authorities: authorities.into_iter().collect(),
            dns: networked,
        },
        endpoints,
        environment,
        unit: UnitProperties {
            supplementary_groups,
            restrict_namespaces: if class == AppClass::NestedUserns {
                ["user", "mnt", "pid", "net", "uts", "ipc"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect()
            } else {
                Vec::new()
            },
            restrict_address_families: address_families,
            private_network: !networked,
            inaccessible_paths: {
                let mut paths = vec![PathBuf::from("/run/dbus/system_bus_socket")];
                // The receipt and staging must be invisible even to a
                // NestedUserns app, which has no Landlock filesystem rules.
                if fetch_enabled {
                    paths.push(state_root.join(".pf-fetch"));
                }
                paths
            },
            read_only_paths: fetch_destinations.into_iter().collect(),
            bind_read_only: read_only_paths.into_iter().collect(),
            bind_read_write: socket_paths.into_iter().collect(),
            bind_read_write_mappings,
            device_allow,
        },
    })
}

const INPUT_DEVICE_DIRECTORY: &str = "/dev/input";

/// `^/dev/input/event[0-9]+$`
fn is_event_node(path: &Path) -> bool {
    path.parent() == Some(Path::new(INPUT_DEVICE_DIRECTORY))
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("event"))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Resolve a declared `[[fetch.files]] dest` under the app's state root.
///
/// The fetcher installs each archive at `<state root>/<dest>`, where `dest` is
/// one relative path component. The manifest parser enforces that rule for
/// schema v3; deriving it here again keeps a hand-built v1/v2 manifest from
/// widening the sandbox. An absent dest is refused rather than defaulted: the
/// fetcher itself requires one, so there is nothing to protect without it.
fn fetch_destination(state_root: &Path, dest: Option<&str>) -> Result<PathBuf, PolicyError> {
    let dest = dest.unwrap_or_default();
    let valid = !dest.is_empty()
        && dest != "."
        && dest != ".."
        && !dest.contains('/')
        && !matches!(dest, "config" | "state" | "cache");
    if !valid {
        return Err(PolicyError(
            "invalid fetch dest: must be one path component outside config, state and cache"
                .to_owned(),
        ));
    }
    Ok(state_root.join(dest))
}

fn confined_absolute(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
}

/// Render only properties derived by [`derive_policy`]. The image renderer embeds this verbatim.
pub fn render_systemd_dropin(policy: &PolicyDecision) -> String {
    let mut lines = vec!["[Service]".to_owned()];
    lines.push(format!(
        "SupplementaryGroups={}",
        policy.unit.supplementary_groups.join(" ")
    ));
    lines.push(if policy.unit.restrict_namespaces.is_empty() {
        "RestrictNamespaces=yes".to_owned()
    } else {
        format!(
            "RestrictNamespaces={}",
            policy.unit.restrict_namespaces.join(" ")
        )
    });
    lines.push(format!(
        "RestrictAddressFamilies={}",
        policy.unit.restrict_address_families.join(" ")
    ));
    lines.push(format!(
        "PrivateNetwork={}",
        if policy.unit.private_network {
            "yes"
        } else {
            "no"
        }
    ));
    for path in &policy.unit.inaccessible_paths {
        // These are deny-only paths: their absence must not make namespace
        // setup fail, while an existing path must still be hidden from apps.
        lines.push(format!("InaccessiblePaths=-{}", path.display()));
    }
    for path in &policy.unit.read_only_paths {
        // The leading '-' keeps a not-yet-fetched destination from failing the
        // unit; when it exists the payload is read-only even against chmod.
        lines.push(format!("ReadOnlyPaths=-{}", path.display()));
    }
    for path in &policy.unit.bind_read_only {
        lines.push(format!("BindReadOnlyPaths={}", path.display()));
    }
    for path in &policy.unit.bind_read_write {
        lines.push(format!("BindPaths={}", path.display()));
    }
    for mapping in &policy.unit.bind_read_write_mappings {
        lines.push(format!(
            "BindPaths={}:{}",
            mapping.source.display(),
            mapping.destination.display()
        ));
    }
    for grant in &policy.unit.device_allow {
        lines.push(format!(
            "DeviceAllow={} {}",
            grant.path.display(),
            grant.mode
        ));
    }
    for authority in &policy.network.authorities {
        let protocol = match authority.protocol {
            EgressProtocol::Tcp => "tcp",
            EgressProtocol::Udp => "udp",
        };
        lines.push(format!(
            "# egress {protocol} {} {}",
            authority.host, authority.port
        ));
    }
    if policy.network.dns {
        lines.push(format!("# derived dns tcp udp {DNS_PORT}"));
    }
    lines.push(String::new());
    lines.join("\n")
}
