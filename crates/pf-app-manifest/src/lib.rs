//! Strict shared parser and fixed-root resolver for image-installed PocketForge applications.
//!
//! Callers provide only a canonical application id. Resolution never accepts a caller-provided
//! descriptor or executable path, and every filesystem object is checked before launch.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

pub const APP_ROOT: &str = "/opt/pocketforge/apps";
pub const PLATFORM_CONTRACT_PATH: &str = "/usr/share/pocketforge/platform-capabilities.toml";

/// The runtime's canonical capability vocabulary.
pub const KNOWN_CAPABILITIES: &[&str] = &[
    "input",
    "vibration",
    "rumble",
    "imu",
    "accelerometer",
    "gyroscope",
    "magnetometer",
    "entropy",
    "location",
    "gnss",
    "audio",
    "settings",
    "leds",
];

/// Minimum-spec graphics version vocabulary: OpenGL ES.
pub const KNOWN_GLES_VERSIONS: &[&str] = &["2.0", "3.0", "3.1", "3.2"];

/// Minimum-spec graphics version vocabulary: Vulkan.
pub const KNOWN_VULKAN_VERSIONS: &[&str] = &["1.0", "1.1", "1.2", "1.3", "1.4"];

/// Minimum-spec input token vocabulary.
pub const KNOWN_INPUTS: &[&str] = &[
    "buttons",
    "dpad",
    "stick",
    "two_sticks",
    "touch",
    "keyboard",
    "pointer",
];

/// Inputs the system input layer provides on every device, so declaring them
/// in a minimum-spec table is redundant and only earns a warning.
pub const SYSTEM_INPUTS: &[&str] = &["keyboard", "pointer"];

/// One normalized `use = [...]` requirement.
///
/// Parsing deliberately matches the capability-policy parser: trim the token, remove one
/// trailing optional marker, split the first modifier separator, trim both components, and
/// normalize only the base capability to lowercase. Semantic validation remains the caller's
/// responsibility.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityRequirement {
    pub base: String,
    pub modifier: Option<String>,
    pub optional: bool,
}

pub fn parse_capability_requirement(token: &str) -> CapabilityRequirement {
    let token = token.trim();
    let (token, optional) = match token.strip_suffix('?') {
        Some(stripped) => (stripped, true),
        None => (token, false),
    };
    let (base, modifier) = match token.split_once(':') {
        Some((base, modifier)) => (
            base.trim().to_ascii_lowercase(),
            Some(modifier.trim().to_owned()),
        ),
        None => (token.trim().to_ascii_lowercase(), None),
    };
    CapabilityRequirement {
        base,
        modifier,
        optional,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub app: App,
    pub runtime: Runtime,
    #[serde(default)]
    pub launch: Option<Launch>,
    #[serde(default)]
    pub health: Option<Health>,
    #[serde(default)]
    pub fetch: Option<Fetch>,
    #[serde(default)]
    pub requirements: Option<Requirements>,
    #[serde(default)]
    pub recommended: Option<Requirements>,
}

/// An app's declared minimum spec (`[requirements]`, enforced at launch) or
/// its informational recommended tier (`[recommended]`, never refuses).
/// Every field is optional; declared fields are validated strictly.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requirements {
    /// Measured peak on the reference device, GPU memory included, no zram credit.
    #[serde(default)]
    pub memory_mib: Option<u32>,
    /// Minimum OpenGL ES version, one of [`KNOWN_GLES_VERSIONS`].
    #[serde(default)]
    pub gles: Option<String>,
    /// Minimum Vulkan version, one of [`KNOWN_VULKAN_VERSIONS`].
    #[serde(default)]
    pub vulkan: Option<String>,
    /// Controls the app cannot work without, from [`KNOWN_INPUTS`].
    #[serde(default)]
    pub inputs: Vec<String>,
    /// Minimum display size in physical pixels, compared orientation-independently.
    #[serde(default)]
    pub display_min: Option<DisplaySize>,
    /// Install size plus working space. Carried and validated, not checked at
    /// launch; sizing installs is the store's job.
    #[serde(default)]
    pub storage_mib: Option<u32>,
}

/// A physical-pixel display size.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisplaySize {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct App {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub category: Option<AppCategory>,
    #[serde(default)]
    pub order: Option<i64>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub upstream_version: Option<String>,
    #[serde(default, rename = "use")]
    pub capabilities: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppCategory {
    Media,
    Stream,
    Game,
    System,
    Settings,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    pub family: String,
    pub abi: String,
    #[serde(default, rename = "platform-version")]
    pub platform_version: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub exec: String,
    #[serde(default)]
    pub needs_network: bool,
    #[serde(default)]
    pub takes_display: bool,
    #[serde(default)]
    pub audio: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Health {
    #[serde(default)]
    pub preflight: Option<String>,
    #[serde(default)]
    pub timeout_sec: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fetch {
    pub enabled: bool,
    #[serde(default)]
    pub destination: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub files: Option<Vec<FetchFile>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchFile {
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub dest: Option<String>,
    #[serde(default)]
    pub strip_components: Option<u64>,
    #[serde(default)]
    pub executable: Option<BTreeMap<String, String>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManifestErrorKind {
    Parse,
    Invalid,
    InvalidLaunchExec,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestError {
    pub kind: ManifestErrorKind,
    pub message: String,
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ManifestError {}

pub fn parse_manifest(source: &str) -> Result<Manifest, ManifestError> {
    let manifest: Manifest = toml::from_str(source).map_err(|error| {
        let message = error.to_string();
        ManifestError {
            kind: if message.contains("unknown field") || message.contains("missing field") {
                ManifestErrorKind::Invalid
            } else {
                ManifestErrorKind::Parse
            },
            message,
        }
    })?;
    validate_manifest(&manifest)?;
    for warning in manifest_warnings(&manifest) {
        eprintln!("pf-app-manifest: warning: {warning}");
    }
    Ok(manifest)
}

fn validate_manifest(manifest: &Manifest) -> Result<(), ManifestError> {
    if validate_app_id(&manifest.app.id).is_err() {
        return invalid_manifest("invalid app.id");
    }
    if !valid_runtime_family(&manifest.runtime.family) {
        return invalid_manifest("invalid runtime.family");
    }
    if !decimal_version(&manifest.runtime.abi) {
        return invalid_manifest("invalid runtime.abi");
    }
    if manifest
        .runtime
        .platform_version
        .as_deref()
        .is_some_and(|version| !decimal_version(version))
    {
        return invalid_manifest("invalid runtime.platform-version");
    }

    let mut capabilities = BTreeSet::new();
    for capability in &manifest.app.capabilities {
        let requirement = parse_capability_requirement(capability);
        if requirement.base.is_empty() || !capabilities.insert(requirement.base.clone()) {
            return invalid_manifest("invalid or duplicate capability");
        }
        if validate_requirement(&requirement).is_err() {
            return invalid_manifest("invalid or duplicate capability");
        }
    }

    if let Some(launch) = &manifest.launch {
        validate_exec_path(&launch.exec).map_err(|message| ManifestError {
            kind: ManifestErrorKind::InvalidLaunchExec,
            message,
        })?;
    }

    for (table, requirements) in [
        ("requirements", manifest.requirements.as_ref()),
        ("recommended", manifest.recommended.as_ref()),
    ] {
        if let Some(requirements) = requirements {
            validate_requirements(table, requirements)?;
        }
    }

    if let Some(fetch) = &manifest.fetch {
        if fetch.enabled && fetch.reason.as_deref().unwrap_or_default().is_empty() {
            return invalid_manifest("enabled fetch requires reason");
        }
        for file in fetch.files.as_deref().unwrap_or_default() {
            if file.sha256.len() != 64
                || !file
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            {
                return invalid_manifest("invalid fetch sha256");
            }
        }
    }
    Ok(())
}

fn validate_requirements(table: &str, requirements: &Requirements) -> Result<(), ManifestError> {
    if requirements
        .gles
        .as_deref()
        .is_some_and(|version| !KNOWN_GLES_VERSIONS.contains(&version))
    {
        return invalid_manifest(&format!("invalid {table}.gles"));
    }
    if requirements
        .vulkan
        .as_deref()
        .is_some_and(|version| !KNOWN_VULKAN_VERSIONS.contains(&version))
    {
        return invalid_manifest(&format!("invalid {table}.vulkan"));
    }
    let mut seen = BTreeSet::new();
    for input in &requirements.inputs {
        if !KNOWN_INPUTS.contains(&input.as_str()) {
            return invalid_manifest(&format!("invalid {table} input"));
        }
        if !seen.insert(input.as_str()) {
            return invalid_manifest(&format!("duplicate {table} input"));
        }
    }
    Ok(())
}

/// Non-fatal advisories for an already-parsed manifest. Today this is exactly
/// the redundant [`SYSTEM_INPUTS`] declarations, which the system input layer
/// satisfies on every device.
pub fn manifest_warnings(manifest: &Manifest) -> Vec<String> {
    let mut warnings = Vec::new();
    for (table, requirements) in [
        ("requirements", manifest.requirements.as_ref()),
        ("recommended", manifest.recommended.as_ref()),
    ] {
        let Some(requirements) = requirements else {
            continue;
        };
        for input in &requirements.inputs {
            if SYSTEM_INPUTS.contains(&input.as_str()) {
                warnings.push(format!(
                    "{table} input {input} is provided by the system input layer on every device"
                ));
            }
        }
    }
    warnings
}

fn invalid_manifest<T>(message: &str) -> Result<T, ManifestError> {
    Err(ManifestError {
        kind: ManifestErrorKind::Invalid,
        message: message.to_owned(),
    })
}

/// Validate the exact application id grammar fixed by the default-app contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidAppId;

impl fmt::Display for InvalidAppId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid application id")
    }
}

impl std::error::Error for InvalidAppId {}

pub fn validate_app_id(id: &str) -> Result<(), InvalidAppId> {
    if !(3..=200).contains(&id.len()) || !id.is_ascii() {
        return Err(InvalidAppId);
    }
    let mut labels = id.split('.');
    let Some(first) = labels.next() else {
        return Err(InvalidAppId);
    };
    let Some(second) = labels.next() else {
        return Err(InvalidAppId);
    };
    if !valid_label(first) || !valid_label(second) || labels.any(|label| !valid_label(label)) {
        return Err(InvalidAppId);
    }
    Ok(())
}

fn valid_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    let edge = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    !bytes.is_empty()
        && edge(bytes[0])
        && edge(bytes[bytes.len() - 1])
        && bytes
            .iter()
            .all(|byte| edge(*byte) || matches!(*byte, b'_' | b'-'))
}

fn valid_runtime_family(value: &str) -> bool {
    let Some(name) = value.strip_prefix("pocketforge/") else {
        return false;
    };
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
}

fn decimal_version(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn validate_requirement(requirement: &CapabilityRequirement) -> Result<(), ()> {
    if requirement.base == "egress" {
        return if requirement
            .modifier
            .as_deref()
            .is_some_and(|modifier| !modifier.is_empty())
        {
            Ok(())
        } else {
            Err(())
        };
    }
    if let Some(modifier) = requirement.modifier.as_deref() {
        if !matches!(
            (requirement.base.as_str(), modifier),
            ("location" | "gnss", "approximate" | "precise")
        ) {
            return Err(());
        }
    }
    if KNOWN_CAPABILITIES.contains(&requirement.base.as_str()) {
        Ok(())
    } else {
        Err(())
    }
}

fn validate_exec_path(value: &str) -> Result<PathBuf, String> {
    if value.is_empty()
        || value.bytes().any(|byte| byte.is_ascii_whitespace())
        || value.bytes().any(|byte| {
            matches!(
                byte,
                b'\0'
                    | b'|'
                    | b'&'
                    | b';'
                    | b'<'
                    | b'>'
                    | b'`'
                    | b'$'
                    | b'\''
                    | b'"'
                    | b'\\'
                    | b'('
                    | b')'
                    | b'{'
                    | b'}'
                    | b'['
                    | b']'
                    | b'*'
                    | b'?'
                    | b'!'
            )
        })
    {
        return Err("launch.exec is not one relative executable path".into());
    }
    let path = Path::new(value);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
        || !path
            .components()
            .any(|component| matches!(component, Component::Normal(_)))
    {
        return Err("launch.exec is not one confined relative path".into());
    }
    Ok(path.to_owned())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformContract {
    pub schema_version: u32,
    pub runtime_family: String,
    pub runtime_abi: String,
    pub platform_version: String,
    pub supported_capabilities: Vec<String>,
    /// Schema 2: total physical memory visible to the platform, in MiB.
    #[serde(default)]
    pub physical_memory_mib: Option<u32>,
    /// Schema 2: the memory budget one app may rely on, in MiB.
    #[serde(default)]
    pub app_memory_budget_mib: Option<u32>,
    /// Schema 2, optional: highest OpenGL ES version passing conformance.
    #[serde(default)]
    pub gles_max: Option<String>,
    /// Schema 2, optional: highest Vulkan version passing conformance.
    #[serde(default)]
    pub vulkan_max: Option<String>,
    /// Schema 2: input vocabulary the device provides.
    #[serde(default)]
    pub inputs: Option<Vec<String>>,
    /// Schema 2: physical display size in pixels.
    #[serde(default)]
    pub display: Option<DisplaySize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformContractErrorReason {
    Missing,
    Read,
    Parse,
    Schema,
    InvalidFamily,
    InvalidAbi,
    InvalidPlatformVersion,
    InvalidCapability,
    DuplicateCapability,
    UnsortedCapabilities,
}

impl PlatformContractErrorReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Read => "read",
            Self::Parse => "parse",
            Self::Schema => "schema",
            Self::InvalidFamily => "invalid_family",
            Self::InvalidAbi => "invalid_abi",
            Self::InvalidPlatformVersion => "invalid_platform_version",
            Self::InvalidCapability => "invalid_capability",
            Self::DuplicateCapability => "duplicate_capability",
            Self::UnsortedCapabilities => "unsorted_capabilities",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlatformContractError {
    pub reason: PlatformContractErrorReason,
    pub message: String,
}

impl fmt::Display for PlatformContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.reason.as_str(), self.message)
    }
}

impl std::error::Error for PlatformContractError {}

pub fn parse_platform_contract(source: &str) -> Result<PlatformContract, PlatformContractError> {
    let contract: PlatformContract = toml::from_str(source).map_err(|error| {
        let message = error.to_string();
        PlatformContractError {
            reason: if message.contains("unknown field") || message.contains("missing field") {
                PlatformContractErrorReason::Schema
            } else {
                PlatformContractErrorReason::Parse
            },
            message,
        }
    })?;
    match contract.schema_version {
        1 => {
            // Under schema 1 every numeric/graphics/input/display fact is absent.
            if contract.physical_memory_mib.is_some()
                || contract.app_memory_budget_mib.is_some()
                || contract.gles_max.is_some()
                || contract.vulkan_max.is_some()
                || contract.inputs.is_some()
                || contract.display.is_some()
            {
                return invalid_contract(
                    PlatformContractErrorReason::Schema,
                    "schema_version 1 must not declare schema 2 fields",
                );
            }
        }
        2 => {
            for (present, field) in [
                (
                    contract.physical_memory_mib.is_some(),
                    "physical_memory_mib",
                ),
                (
                    contract.app_memory_budget_mib.is_some(),
                    "app_memory_budget_mib",
                ),
                (contract.inputs.is_some(), "inputs"),
                (contract.display.is_some(), "display"),
            ] {
                if !present {
                    return invalid_contract(
                        PlatformContractErrorReason::Schema,
                        &format!("schema_version 2 requires {field}"),
                    );
                }
            }
            if contract
                .gles_max
                .as_deref()
                .is_some_and(|version| !KNOWN_GLES_VERSIONS.contains(&version))
            {
                return invalid_contract(PlatformContractErrorReason::Schema, "invalid gles_max");
            }
            if contract
                .vulkan_max
                .as_deref()
                .is_some_and(|version| !KNOWN_VULKAN_VERSIONS.contains(&version))
            {
                return invalid_contract(PlatformContractErrorReason::Schema, "invalid vulkan_max");
            }
            for input in contract.inputs.as_deref().unwrap_or_default() {
                if !KNOWN_INPUTS.contains(&input.as_str()) {
                    return invalid_contract(
                        PlatformContractErrorReason::Schema,
                        "unsupported input name",
                    );
                }
            }
        }
        _ => {
            return invalid_contract(
                PlatformContractErrorReason::Schema,
                "schema_version must be 1 or 2",
            );
        }
    }
    if !valid_runtime_family(&contract.runtime_family) {
        return invalid_contract(
            PlatformContractErrorReason::InvalidFamily,
            "invalid runtime_family",
        );
    }
    if !decimal_version(&contract.runtime_abi) {
        return invalid_contract(
            PlatformContractErrorReason::InvalidAbi,
            "invalid runtime_abi",
        );
    }
    if !decimal_version(&contract.platform_version) {
        return invalid_contract(
            PlatformContractErrorReason::InvalidPlatformVersion,
            "invalid platform_version",
        );
    }
    let mut previous: Option<&str> = None;
    let mut seen = BTreeSet::new();
    for capability in &contract.supported_capabilities {
        if !KNOWN_CAPABILITIES.contains(&capability.as_str())
            || capability.contains(':')
            || capability.ends_with('?')
        {
            return invalid_contract(
                PlatformContractErrorReason::InvalidCapability,
                "unsupported capability name",
            );
        }
        if !seen.insert(capability.as_str()) {
            return invalid_contract(
                PlatformContractErrorReason::DuplicateCapability,
                "duplicate supported capability",
            );
        }
        if previous.is_some_and(|value| value > capability.as_str()) {
            return invalid_contract(
                PlatformContractErrorReason::UnsortedCapabilities,
                "supported capabilities must be sorted",
            );
        }
        previous = Some(capability);
    }
    Ok(contract)
}

fn invalid_contract<T>(
    reason: PlatformContractErrorReason,
    message: &str,
) -> Result<T, PlatformContractError> {
    Err(PlatformContractError {
        reason,
        message: message.to_owned(),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReasonCode {
    InvalidId,
    AppRootMissing,
    AppRootSymlink,
    AppNotFound,
    AppDirNotDirectory,
    AppDirSymlink,
    AppDirEscape,
    DescriptorMissing,
    DescriptorNotRegular,
    DescriptorSymlink,
    DescriptorParse,
    DescriptorInvalid,
    DescriptorIdMismatch,
    LaunchMissing,
    LaunchExecInvalid,
    ExecMissing,
    ExecNotRegular,
    ExecSymlink,
    ExecEscape,
    ExecNotExecutable,
    ExecIo,
    RuntimeFamilyMismatch,
    RuntimeAbiMismatch,
    PlatformVersionMismatch,
    UnsupportedCapability,
    RequirementUnverifiable,
    InsufficientMemory,
    UnsupportedGraphics,
    MissingInput,
    DisplayTooSmall,
    PlatformContractMissing,
    PlatformContractInvalid,
    SystemdStartFailed,
    SystemdStateUnknown,
    AppExitFailed,
    TargetNotReleased,
    OwnerNotActive,
    PresentationNotAcknowledged,
}

impl ReasonCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidId => "invalid_id",
            Self::AppRootMissing => "app_root_missing",
            Self::AppRootSymlink => "app_root_symlink",
            Self::AppNotFound => "app_not_found",
            Self::AppDirNotDirectory => "app_dir_not_directory",
            Self::AppDirSymlink => "app_dir_symlink",
            Self::AppDirEscape => "app_dir_escape",
            Self::DescriptorMissing => "descriptor_missing",
            Self::DescriptorNotRegular => "descriptor_not_regular",
            Self::DescriptorSymlink => "descriptor_symlink",
            Self::DescriptorParse => "descriptor_parse",
            Self::DescriptorInvalid => "descriptor_invalid",
            Self::DescriptorIdMismatch => "descriptor_id_mismatch",
            Self::LaunchMissing => "launch_missing",
            Self::LaunchExecInvalid => "launch_exec_invalid",
            Self::ExecMissing => "exec_missing",
            Self::ExecNotRegular => "exec_not_regular",
            Self::ExecSymlink => "exec_symlink",
            Self::ExecEscape => "exec_escape",
            Self::ExecNotExecutable => "exec_not_executable",
            Self::ExecIo => "exec_io",
            Self::RuntimeFamilyMismatch => "runtime_family_mismatch",
            Self::RuntimeAbiMismatch => "runtime_abi_mismatch",
            Self::PlatformVersionMismatch => "platform_version_mismatch",
            Self::UnsupportedCapability => "unsupported_capability",
            Self::RequirementUnverifiable => "requirement_unverifiable",
            Self::InsufficientMemory => "insufficient_memory",
            Self::UnsupportedGraphics => "unsupported_graphics",
            Self::MissingInput => "missing_input",
            Self::DisplayTooSmall => "display_too_small",
            Self::PlatformContractMissing => "platform_contract_missing",
            Self::PlatformContractInvalid => "platform_contract_invalid",
            Self::SystemdStartFailed => "systemd_start_failed",
            Self::SystemdStateUnknown => "systemd_state_unknown",
            Self::AppExitFailed => "app_exit_failed",
            Self::TargetNotReleased => "target_not_released",
            Self::OwnerNotActive => "owner_not_active",
            Self::PresentationNotAcknowledged => "presentation_not_acknowledged",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolveError {
    pub reason: ReasonCode,
    pub detail: String,
    exit_code: u8,
}

impl ResolveError {
    pub const fn helper_exit_code(&self) -> u8 {
        self.exit_code
    }
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.reason.as_str(), self.detail)
    }
}

impl std::error::Error for ResolveError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedApp {
    pub id: String,
    pub app_dir: PathBuf,
    pub executable: PathBuf,
    pub manifest: Manifest,
    pub platform: PlatformContract,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Resolver {
    app_root: PathBuf,
    platform_contract: PathBuf,
}

impl Default for Resolver {
    fn default() -> Self {
        Self::fixed()
    }
}

impl Resolver {
    pub fn fixed() -> Self {
        Self::new(APP_ROOT, PLATFORM_CONTRACT_PATH)
    }

    /// Inject filesystem roots for hermetic tests. Production callers use [`Self::fixed`].
    pub fn new(app_root: impl Into<PathBuf>, platform_contract: impl Into<PathBuf>) -> Self {
        Self {
            app_root: app_root.into(),
            platform_contract: platform_contract.into(),
        }
    }

    pub fn resolve(&self, id: &str) -> Result<ResolvedApp, ResolveError> {
        if validate_app_id(id).is_err() {
            return Err(resolve_error(
                ReasonCode::InvalidId,
                65,
                "invalid application id",
            ));
        }

        let root_metadata = symlink_metadata(
            &self.app_root,
            ReasonCode::AppRootMissing,
            "application root",
        )?;
        if root_metadata.file_type().is_symlink() {
            return Err(resolve_error(
                ReasonCode::AppRootSymlink,
                66,
                "application root is a symlink",
            ));
        }
        if !root_metadata.is_dir() {
            return Err(resolve_error(
                ReasonCode::AppRootMissing,
                66,
                "application root is not a directory",
            ));
        }
        let canonical_root = fs::canonicalize(&self.app_root).map_err(|error| {
            io_resolve_error(
                ReasonCode::AppRootMissing,
                "canonicalize application root",
                error,
            )
        })?;

        let app_dir = self.app_root.join(id);
        let app_metadata = symlink_metadata(&app_dir, ReasonCode::AppNotFound, "application")?;
        if app_metadata.file_type().is_symlink() {
            return Err(resolve_error(
                ReasonCode::AppDirSymlink,
                66,
                "application directory is a symlink",
            ));
        }
        if !app_metadata.is_dir() {
            return Err(resolve_error(
                ReasonCode::AppDirNotDirectory,
                66,
                "application path is not a directory",
            ));
        }
        let canonical_app = fs::canonicalize(&app_dir).map_err(|error| {
            io_resolve_error(ReasonCode::AppDirEscape, "canonicalize application", error)
        })?;
        if canonical_app.parent() != Some(canonical_root.as_path()) {
            return Err(resolve_error(
                ReasonCode::AppDirEscape,
                65,
                "application directory escaped the fixed root",
            ));
        }

        let descriptor = canonical_app.join("app.toml");
        let descriptor_metadata =
            symlink_metadata(&descriptor, ReasonCode::DescriptorMissing, "descriptor")?;
        if descriptor_metadata.file_type().is_symlink() {
            return Err(resolve_error(
                ReasonCode::DescriptorSymlink,
                66,
                "descriptor is a symlink",
            ));
        }
        if !descriptor_metadata.is_file() {
            return Err(resolve_error(
                ReasonCode::DescriptorNotRegular,
                66,
                "descriptor is not a regular file",
            ));
        }
        let source = fs::read_to_string(&descriptor).map_err(|error| ResolveError {
            reason: ReasonCode::DescriptorParse,
            detail: format!("read descriptor: {error}"),
            exit_code: 74,
        })?;
        let manifest = parse_manifest(&source).map_err(|error| match error.kind {
            ManifestErrorKind::Parse => {
                resolve_error(ReasonCode::DescriptorParse, 65, error.message)
            }
            ManifestErrorKind::Invalid => {
                resolve_error(ReasonCode::DescriptorInvalid, 65, error.message)
            }
            ManifestErrorKind::InvalidLaunchExec => {
                resolve_error(ReasonCode::LaunchExecInvalid, 65, error.message)
            }
        })?;
        if manifest.app.id != id {
            return Err(resolve_error(
                ReasonCode::DescriptorIdMismatch,
                65,
                "descriptor app.id does not match requested id",
            ));
        }
        let launch = manifest.launch.as_ref().ok_or_else(|| {
            resolve_error(
                ReasonCode::LaunchMissing,
                65,
                "descriptor has no launch table",
            )
        })?;
        let relative_exec = validate_exec_path(&launch.exec)
            .map_err(|message| resolve_error(ReasonCode::LaunchExecInvalid, 65, message))?;
        let executable = canonical_app.join(relative_exec);
        let executable_metadata =
            symlink_metadata(&executable, ReasonCode::ExecMissing, "executable")?;
        if executable_metadata.file_type().is_symlink() {
            return Err(resolve_error(
                ReasonCode::ExecSymlink,
                66,
                "executable is a symlink",
            ));
        }
        if !executable_metadata.file_type().is_file()
            || executable_metadata.file_type().is_block_device()
            || executable_metadata.file_type().is_char_device()
            || executable_metadata.file_type().is_fifo()
            || executable_metadata.file_type().is_socket()
        {
            return Err(resolve_error(
                ReasonCode::ExecNotRegular,
                66,
                "executable is not a regular file",
            ));
        }
        let canonical_exec = fs::canonicalize(&executable).map_err(|error| {
            io_resolve_error(ReasonCode::ExecIo, "canonicalize executable", error)
        })?;
        if !canonical_exec.starts_with(&canonical_app) {
            return Err(resolve_error(
                ReasonCode::ExecEscape,
                65,
                "executable escaped the application directory",
            ));
        }
        if executable_metadata.permissions().mode() & 0o111 == 0 {
            return Err(resolve_error(
                ReasonCode::ExecNotExecutable,
                126,
                "executable has no execute bit",
            ));
        }

        let platform_source = fs::read_to_string(&self.platform_contract).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                resolve_error(
                    ReasonCode::PlatformContractMissing,
                    65,
                    "platform contract is missing",
                )
            } else {
                resolve_error(
                    ReasonCode::PlatformContractInvalid,
                    74,
                    format!("read platform contract: {error}"),
                )
            }
        })?;
        let platform = parse_platform_contract(&platform_source).map_err(|error| {
            resolve_error(ReasonCode::PlatformContractInvalid, 65, error.to_string())
        })?;
        check_compatibility(&manifest, &platform)?;

        Ok(ResolvedApp {
            id: id.to_owned(),
            app_dir: canonical_app,
            executable: canonical_exec,
            manifest,
            platform,
        })
    }
}

fn check_compatibility(
    manifest: &Manifest,
    platform: &PlatformContract,
) -> Result<(), ResolveError> {
    if manifest.runtime.family != platform.runtime_family {
        return Err(resolve_error(
            ReasonCode::RuntimeFamilyMismatch,
            65,
            "application runtime family does not match platform",
        ));
    }
    if manifest.runtime.abi != platform.runtime_abi {
        return Err(resolve_error(
            ReasonCode::RuntimeAbiMismatch,
            65,
            "application runtime ABI does not match platform",
        ));
    }
    if manifest
        .runtime
        .platform_version
        .as_deref()
        .is_some_and(|version| version != platform.platform_version)
    {
        return Err(resolve_error(
            ReasonCode::PlatformVersionMismatch,
            65,
            "application platform version does not match platform",
        ));
    }
    for capability in &manifest.app.capabilities {
        let requirement = parse_capability_requirement(capability);
        if !requirement.optional
            && requirement.base != "egress"
            && !platform
                .supported_capabilities
                .iter()
                .any(|capability| capability == &requirement.base)
        {
            return Err(resolve_error(
                ReasonCode::UnsupportedCapability,
                65,
                format!("unsupported required capability {}", requirement.base),
            ));
        }
    }
    if let Some(requirements) = &manifest.requirements {
        check_requirements(requirements, platform)?;
    }
    Ok(())
}

/// Enforce the `[requirements]` minimum spec against the platform contract's
/// device facts. A contract that lacks the fact for a declared requirement
/// fails closed with [`ReasonCode::RequirementUnverifiable`]; under schema 1
/// every fact is absent, so any requirement is unverifiable there. The
/// `[recommended]` table is informational and is never consulted here.
fn check_requirements(
    requirements: &Requirements,
    platform: &PlatformContract,
) -> Result<(), ResolveError> {
    if let Some(required) = requirements.memory_mib {
        match platform.app_memory_budget_mib {
            None => {
                return Err(unverifiable(format!(
                    "memory_mib {required} is unverifiable: platform contract declares no app_memory_budget_mib"
                )))
            }
            Some(budget) if required > budget => {
                return Err(resolve_error(
                    ReasonCode::InsufficientMemory,
                    65,
                    format!("memory_mib {required} exceeds app_memory_budget_mib {budget}"),
                ))
            }
            Some(_) => {}
        }
    }
    if let Some(required) = requirements.gles.as_deref() {
        check_graphics(
            "gles",
            required,
            platform.gles_max.as_deref(),
            KNOWN_GLES_VERSIONS,
        )?;
    }
    if let Some(required) = requirements.vulkan.as_deref() {
        check_graphics(
            "vulkan",
            required,
            platform.vulkan_max.as_deref(),
            KNOWN_VULKAN_VERSIONS,
        )?;
    }
    for input in &requirements.inputs {
        if SYSTEM_INPUTS.contains(&input.as_str()) {
            continue;
        }
        let Some(device_inputs) = platform.inputs.as_deref() else {
            return Err(unverifiable(format!(
                "input {input} is unverifiable: platform contract declares no inputs"
            )));
        };
        if !input_satisfied(input, device_inputs) {
            return Err(resolve_error(
                ReasonCode::MissingInput,
                65,
                format!(
                    "required input {input} is not among platform inputs {}",
                    device_inputs.join(", ")
                ),
            ));
        }
    }
    if let Some(required) = requirements.display_min {
        let Some(display) = platform.display else {
            return Err(unverifiable(format!(
                "display_min {}x{} is unverifiable: platform contract declares no display",
                required.width, required.height
            )));
        };
        let (required_small, required_large) = sorted_sides(required);
        let (display_small, display_large) = sorted_sides(display);
        if required_small > display_small || required_large > display_large {
            return Err(resolve_error(
                ReasonCode::DisplayTooSmall,
                65,
                format!(
                    "display_min {}x{} exceeds display {}x{}",
                    required.width, required.height, display.width, display.height
                ),
            ));
        }
    }
    Ok(())
}

/// Rank both versions in the fixed vocabulary and refuse when the required
/// version outranks the platform maximum. An unrankable maximum (absent or
/// outside the vocabulary) fails closed as unverifiable.
fn check_graphics(
    name: &str,
    required: &str,
    maximum: Option<&str>,
    vocabulary: &[&str],
) -> Result<(), ResolveError> {
    let rank = |value: &str| vocabulary.iter().position(|known| *known == value);
    match (rank(required), maximum, maximum.and_then(rank)) {
        (Some(required_rank), Some(maximum), Some(maximum_rank)) => {
            if required_rank > maximum_rank {
                return Err(resolve_error(
                    ReasonCode::UnsupportedGraphics,
                    65,
                    format!("{name} {required} exceeds {name}_max {maximum}"),
                ));
            }
            Ok(())
        }
        _ => Err(unverifiable(format!(
            "{name} {required} is unverifiable: platform contract declares no {name}_max"
        ))),
    }
}

/// `two_sticks` implies `stick`; every other token must be declared as-is.
fn input_satisfied(required: &str, device_inputs: &[String]) -> bool {
    device_inputs
        .iter()
        .any(|input| input == required || (required == "stick" && input == "two_sticks"))
}

/// The smaller side first, so display comparisons are orientation-independent.
fn sorted_sides(display: DisplaySize) -> (u32, u32) {
    if display.width <= display.height {
        (display.width, display.height)
    } else {
        (display.height, display.width)
    }
}

fn unverifiable(detail: String) -> ResolveError {
    resolve_error(ReasonCode::RequirementUnverifiable, 65, detail)
}

fn symlink_metadata(
    path: &Path,
    missing_reason: ReasonCode,
    label: &str,
) -> Result<fs::Metadata, ResolveError> {
    fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            resolve_error(missing_reason, 66, format!("{label} not found"))
        } else {
            io_resolve_error(missing_reason, format!("inspect {label}"), error)
        }
    })
}

fn io_resolve_error(
    reason: ReasonCode,
    operation: impl fmt::Display,
    error: std::io::Error,
) -> ResolveError {
    ResolveError {
        reason,
        detail: format!("{operation}: {error}"),
        exit_code: 74,
    }
}

fn resolve_error(reason: ReasonCode, exit_code: u8, detail: impl Into<String>) -> ResolveError {
    ResolveError {
        reason,
        detail: detail.into(),
        exit_code,
    }
}

#[cfg(test)]
mod tests;
