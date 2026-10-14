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
pub struct UnitProperties {
    pub supplementary_groups: Vec<String>,
    pub restrict_namespaces: Vec<String>,
    pub restrict_address_families: Vec<String>,
    pub private_network: bool,
    pub inaccessible_paths: Vec<PathBuf>,
    pub bind_read_only: Vec<PathBuf>,
    pub bind_read_write: Vec<PathBuf>,
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
        for path in &inputs.resource_paths {
            filesystem.push(FsRule {
                path: path.clone(),
                access: FsAccess::ReadExecute,
            });
        }
    }

    let mut read_only_paths = BTreeSet::new();
    let mut socket_paths = BTreeSet::new();
    let mut devices: BTreeMap<PathBuf, String> = BTreeMap::new();
    let mut groups = BTreeSet::new();
    let resource = |name: &str, optional: bool| -> Result<Vec<PathBuf>, PolicyError> {
        match inputs.resources.get(name) {
            Some(paths) if !paths.is_empty() => Ok(paths.clone()),
            _ if optional => Ok(Vec::new()),
            _ => Err(PolicyError(format!("required resource {name} is missing"))),
        }
    };
    let mut add_devices = |name: &str, mode: &str, optional: bool| -> Result<(), PolicyError> {
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

    if launch.takes_display {
        groups.insert("video".to_owned());
        socket_paths.extend(resource("display-sockets", false)?);
        read_only_paths.extend(resource("session-environment", false)?);
        add_devices("render-devices", "rw", false)?;
    }
    if launch.audio || capability_bases.contains("audio") {
        groups.insert("audio".to_owned());
        let optional = !launch.audio && capability_optional.get("audio") == Some(&true);
        socket_paths.extend(resource("audio-sockets", optional)?);
        read_only_paths.extend(resource("session-environment", optional)?);
        add_devices("audio-devices", "rw", optional)?;
    }
    if capability_bases.contains("input") {
        groups.insert("input".to_owned());
        add_devices(
            "input-devices",
            "r",
            capability_optional.get("input") == Some(&true),
        )?;
    }
    if capability_bases.contains("rumble") || capability_bases.contains("vibration") {
        groups.insert("input".to_owned());
        let optional = ["rumble", "vibration"]
            .into_iter()
            .filter(|name| capability_bases.contains(*name))
            .all(|name| capability_optional.get(name) == Some(&true));
        add_devices("input-devices", "rw", optional)?;
    }
    if capability_bases.contains("video-decode") {
        let optional = capability_optional.get("video-decode") == Some(&true);
        add_devices("video-decode-devices", "rw", optional)?;
        add_devices("media-devices", "rw", optional)?;
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
        socket_paths.extend(resource("broker-sockets", optional)?);
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
        filesystem.extend(devices.iter().map(|(path, mode)| FsRule {
            path: path.clone(),
            access: if mode == "rw" {
                FsAccess::DeviceReadWrite
            } else {
                FsAccess::DeviceReadOnly
            },
        }));
    }
    for (path, mode) in &devices {
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

    // C3: input groups/devices intentionally preserve today's raw grant. The input mediation
    // bead changes the mapping above in this compiler, without adding a second policy list.
    let supplementary_groups = groups.into_iter().collect();
    let device_allow = devices
        .into_iter()
        .map(|(path, mode)| DeviceGrant { path, mode })
        .collect();

    let mut endpoints = Vec::new();
    if capability_bases.contains("bluetooth") {
        endpoints.push(MediatedEndpoint::BluetoothPort);
    }
    let mut address_families = vec!["AF_UNIX".to_owned()];
    if networked {
        address_families.extend(["AF_INET".to_owned(), "AF_INET6".to_owned()]);
    }
    if class == AppClass::NestedUserns {
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
            inaccessible_paths: vec![PathBuf::from("/run/dbus/system_bus_socket")],
            bind_read_only: read_only_paths.into_iter().collect(),
            bind_read_write: socket_paths.into_iter().collect(),
            device_allow,
        },
    })
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
        lines.push(format!("InaccessiblePaths={}", path.display()));
    }
    for path in &policy.unit.bind_read_only {
        lines.push(format!("BindReadOnlyPaths={}", path.display()));
    }
    for path in &policy.unit.bind_read_write {
        lines.push(format!("BindPaths={}", path.display()));
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
