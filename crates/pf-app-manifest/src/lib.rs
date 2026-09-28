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
        if !capabilities.insert(capability) || validate_requirement(capability).is_err() {
            return invalid_manifest("invalid or duplicate capability");
        }
    }

    if let Some(launch) = &manifest.launch {
        validate_exec_path(&launch.exec).map_err(|message| ManifestError {
            kind: ManifestErrorKind::InvalidLaunchExec,
            message,
        })?;
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

fn validate_requirement(value: &str) -> Result<(), ()> {
    let value = value.strip_suffix('?').unwrap_or(value);
    if value.is_empty() || value.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err(());
    }
    let (base, modifier) = match value.split_once(':') {
        Some(parts) => parts,
        None => (value, ""),
    };
    if base == "egress" {
        return if !modifier.is_empty() && !modifier.contains(':') {
            Ok(())
        } else {
            Err(())
        };
    }
    if !modifier.is_empty()
        && !matches!(
            (base, modifier),
            ("location" | "gnss", "approximate" | "precise")
        )
    {
        return Err(());
    }
    if KNOWN_CAPABILITIES.contains(&base) {
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
    if contract.schema_version != 1 {
        return invalid_contract(
            PlatformContractErrorReason::Schema,
            "schema_version must be 1",
        );
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
    for requirement in &manifest.app.capabilities {
        let required = !requirement.ends_with('?');
        let normalized = requirement.strip_suffix('?').unwrap_or(requirement);
        let base = normalized
            .split_once(':')
            .map_or(normalized, |parts| parts.0);
        if required
            && base != "egress"
            && !platform
                .supported_capabilities
                .iter()
                .any(|capability| capability == base)
        {
            return Err(resolve_error(
                ReasonCode::UnsupportedCapability,
                65,
                format!("unsupported required capability {base}"),
            ));
        }
    }
    Ok(())
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
