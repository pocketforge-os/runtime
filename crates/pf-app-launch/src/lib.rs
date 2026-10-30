//! Library seam for the `pf-app-launch <app-id>` helper.

pub mod landlock;
pub mod policy;

use crate::landlock::{apply_landlock, AppliedPolicy, LandlockError, LandlockKernel};
use crate::policy::{
    derive_policy, PolicyDecision, PolicyError, PolicyInputs, PolicyResourceInventory,
};
use pf_app_manifest::{ReasonCode, ResolveError, Resolver};
use std::ffi::CString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub trait Exec {
    /// Replace the current process. A successful production call never returns.
    fn exec(
        &mut self,
        executable: &Path,
        working_directory: &Path,
        environment: &[(String, String)],
    ) -> io::Result<()>;
}

#[derive(Default)]
pub struct ProcessExec;

impl Exec for ProcessExec {
    fn exec(
        &mut self,
        executable: &Path,
        working_directory: &Path,
        environment: &[(String, String)],
    ) -> io::Result<()> {
        let error = execution_command(executable, working_directory, environment).exec();
        Err(error)
    }
}

fn execution_command(
    executable: &Path,
    working_directory: &Path,
    environment: &[(String, String)],
) -> Command {
    let mut command = Command::new(executable);
    command
        .current_dir(working_directory)
        // DISPLAY is never ambient: every child starts with it removed and only the
        // manifest-derived projection below can add the validated value back.
        .env_remove("DISPLAY")
        // The base unit supplies the broker socket for compatibility, but the signed
        // resource inventory decides whether this launch uses the broker or raw input.
        .env_remove("PF_BROKER_SOCK")
        .envs(environment.iter().cloned());
    command
}

#[derive(Debug)]
pub enum LaunchError {
    Resolve(ResolveError),
    Xdg(io::Error),
    Policy(PolicyError),
    Landlock(LandlockError),
    Exec(io::Error),
}

impl LaunchError {
    pub const fn reason(&self) -> ReasonCode {
        match self {
            Self::Resolve(error) => error.reason,
            Self::Xdg(_) | Self::Exec(_) => ReasonCode::ExecIo,
            Self::Policy(_) => ReasonCode::PolicyInvalid,
            Self::Landlock(LandlockError::Unavailable(_)) => ReasonCode::LandlockUnavailable,
            Self::Landlock(LandlockError::AbiTooOld { .. }) => ReasonCode::LandlockAbiTooOld,
            Self::Landlock(LandlockError::Apply(_)) => ReasonCode::PolicyApplyFailed,
        }
    }

    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::Resolve(error) => error.helper_exit_code(),
            Self::Xdg(_) => 74,
            Self::Policy(_) => 65,
            Self::Landlock(_) => 77,
            Self::Exec(_) => 126,
        }
    }
}

impl fmt::Display for LaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resolve(error) => error.fmt(f),
            Self::Xdg(error) => write!(f, "XDG directory: {error}"),
            Self::Policy(error) => write!(f, "policy: {error}"),
            Self::Landlock(error) => error.fmt(f),
            Self::Exec(error) => write!(f, "execve: {error}"),
        }
    }
}

impl std::error::Error for LaunchError {}

pub trait PolicyEnforcer {
    fn enforce_policy(&mut self, policy: &PolicyDecision) -> Result<AppliedPolicy, LandlockError>;
}

impl<T: LandlockKernel> PolicyEnforcer for T {
    fn enforce_policy(&mut self, policy: &PolicyDecision) -> Result<AppliedPolicy, LandlockError> {
        apply_landlock(policy, self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchPaths {
    pub xdg_config: PathBuf,
    pub xdg_state: PathBuf,
    pub xdg_cache: PathBuf,
    pub platform_runtime: PathBuf,
    pub resource_paths: Vec<PathBuf>,
    pub resources: std::collections::BTreeMap<String, Vec<PathBuf>>,
}

fn platform_resource_paths() -> Vec<PathBuf> {
    [
        "/usr",
        "/lib",
        "/lib64",
        "/bin",
        "/sbin",
        "/etc",
        "/opt/pocketforge/lib",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

/// Resolve, compile and enforce policy, then execute an installed application.
pub fn launch_with<E: Exec, P: PolicyEnforcer>(
    resolver: &Resolver,
    app_id: &str,
    paths: &LaunchPaths,
    enforcer: &mut P,
    executor: &mut E,
) -> Result<(), LaunchError> {
    let resolved = resolver.resolve(app_id).map_err(LaunchError::Resolve)?;
    for directory in [&paths.xdg_config, &paths.xdg_state, &paths.xdg_cache] {
        fs::create_dir_all(directory).map_err(LaunchError::Xdg)?;
    }
    let policy = derive_policy(
        &resolved,
        &PolicyInputs {
            platform_runtime: paths.platform_runtime.clone(),
            xdg_config: paths.xdg_config.clone(),
            xdg_state: paths.xdg_state.clone(),
            xdg_cache: paths.xdg_cache.clone(),
            resource_paths: paths.resource_paths.clone(),
            resources: paths.resources.clone(),
        },
    )
    .map_err(LaunchError::Policy)?;
    let environment = load_environment(&policy).map_err(LaunchError::Policy)?;
    enforcer
        .enforce_policy(&policy)
        .map_err(LaunchError::Landlock)?;
    executor
        .exec(&resolved.executable, &resolved.app_dir, &environment)
        .map_err(LaunchError::Exec)
}

fn load_environment(policy: &PolicyDecision) -> Result<Vec<(String, String)>, PolicyError> {
    let mut environment = Vec::new();
    if policy
        .endpoints
        .contains(&policy::MediatedEndpoint::InputBroker)
    {
        let socket = Path::new(policy::INPUT_BROKER_SOCKET);
        if !policy
            .unit
            .bind_read_write
            .iter()
            .any(|path| path == socket)
        {
            return Err(PolicyError(
                "input broker endpoint has no canonical socket projection".to_owned(),
            ));
        }
        environment.push((
            "PF_BROKER_SOCK".to_owned(),
            policy::INPUT_BROKER_SOCKET.to_owned(),
        ));
    }
    for projection in &policy.environment {
        if projection.name == "DISPLAY" {
            let socket = policy
                .unit
                .bind_read_write_mappings
                .iter()
                .find(|mapping| mapping.source == Path::new(policy::X11_SOCKET))
                .ok_or_else(|| PolicyError("x11 DISPLAY has no socket projection".to_owned()))?;
            let socket_file = open_without_symlinks(
                &socket.destination,
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                "inspect x11 socket",
            )?;
            let metadata = socket_file.metadata().map_err(|error| {
                PolicyError(format!(
                    "inspect x11 socket {}: {error}",
                    socket.destination.display()
                ))
            })?;
            if metadata.file_type().is_symlink() {
                return Err(PolicyError(format!(
                    "x11 socket {} is an alias",
                    socket.destination.display()
                )));
            }
            if !metadata.file_type().is_socket() {
                return Err(PolicyError(format!(
                    "x11 socket {} is not a filesystem socket",
                    socket.destination.display()
                )));
            }
        }
        let mut source_file = open_without_symlinks(
            &projection.source,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            "read capability environment",
        )?;
        if !source_file
            .metadata()
            .map_err(|error| {
                PolicyError(format!(
                    "inspect capability environment {}: {error}",
                    projection.source.display()
                ))
            })?
            .file_type()
            .is_file()
        {
            return Err(PolicyError(format!(
                "capability environment {} is not a regular file",
                projection.source.display()
            )));
        }
        let mut source = String::new();
        source_file.read_to_string(&mut source).map_err(|error| {
            PolicyError(format!(
                "read capability environment {}: {error}",
                projection.source.display()
            ))
        })?;
        let expected = format!("{}={}\n", projection.name, projection.expected_value);
        if source != expected {
            return Err(PolicyError(format!(
                "capability environment {} is malformed",
                projection.source.display()
            )));
        }
        environment.push((projection.name.clone(), projection.expected_value.clone()));
    }
    Ok(environment)
}

fn open_without_symlinks(
    path: &Path,
    final_flags: i32,
    operation: &str,
) -> Result<File, PolicyError> {
    let mut components = path.components();
    if !matches!(components.next(), Some(std::path::Component::RootDir)) {
        return Err(PolicyError(format!(
            "{operation} {}: path is not absolute",
            path.display()
        )));
    }
    let components = components.collect::<Vec<_>>();
    if components.is_empty() {
        return Err(PolicyError(format!(
            "{operation} {}: path has no object component",
            path.display()
        )));
    }

    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open("/")
        .map_err(|error| PolicyError(format!("{operation} {}: {error}", path.display())))?;
    let mut directory = OwnedFd::from(root);

    for (index, component) in components.iter().enumerate() {
        let std::path::Component::Normal(name) = component else {
            return Err(PolicyError(format!(
                "{operation} {}: path contains an alias or invalid component",
                path.display()
            )));
        };
        let name = CString::new(name.as_bytes()).map_err(|_| {
            PolicyError(format!(
                "{operation} {}: path contains an invalid component",
                path.display()
            ))
        })?;
        let final_component = index + 1 == components.len();
        let flags = if final_component {
            final_flags
        } else {
            libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        };
        // SAFETY: `directory` and `name` remain live for the call, and a successful returned
        // descriptor is immediately moved into `OwnedFd` for single-owner close semantics.
        let raw_fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if raw_fd < 0 {
            let error = io::Error::last_os_error();
            let detail = if matches!(
                error.raw_os_error(),
                Some(code) if code == libc::ELOOP || code == libc::ENOTDIR
            ) {
                "path contains an alias or non-directory component".to_owned()
            } else {
                error.to_string()
            };
            return Err(PolicyError(format!(
                "{operation} {}: {detail}",
                path.display()
            )));
        }
        // SAFETY: `openat` returned a new owned descriptor above.
        let opened = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        if final_component {
            return Ok(File::from(opened));
        }
        directory = opened;
    }
    unreachable!("non-empty path components return from the final iteration")
}

pub fn required_launch_paths(app_id: &str) -> Result<LaunchPaths, LaunchError> {
    let state_root = PathBuf::from("/var/lib/pocketforge/apps").join(app_id);
    let inventory_path =
        PathBuf::from("/usr/share/pocketforge/app-policy").join(format!("{app_id}.resources.json"));
    let inventory_source = fs::read_to_string(&inventory_path).map_err(|error| {
        LaunchError::Policy(PolicyError(format!(
            "read signed resource inventory {}: {error}",
            inventory_path.display()
        )))
    })?;
    let inventory: PolicyResourceInventory =
        serde_json::from_str(&inventory_source).map_err(|error| {
            LaunchError::Policy(PolicyError(format!(
                "parse signed resource inventory {}: {error}",
                inventory_path.display()
            )))
        })?;
    required_environment_path("XDG_RUNTIME_DIR", PathBuf::from("/run/pf-app").join(app_id))?;
    Ok(LaunchPaths {
        xdg_config: required_environment_path("XDG_CONFIG_HOME", state_root.join("config"))?,
        xdg_state: required_environment_path("XDG_STATE_HOME", state_root.join("state"))?,
        xdg_cache: required_environment_path(
            "XDG_CACHE_HOME",
            PathBuf::from("/var/cache/pocketforge/apps").join(app_id),
        )?,
        platform_runtime: inventory.platform_runtime,
        resource_paths: platform_resource_paths(),
        resources: inventory.resources,
    })
}

fn required_environment_path(name: &str, expected: PathBuf) -> Result<PathBuf, LaunchError> {
    validate_environment_path(name, std::env::var_os(name).map(PathBuf::from), expected)
}

fn validate_environment_path(
    name: &str,
    actual: Option<PathBuf>,
    expected: PathBuf,
) -> Result<PathBuf, LaunchError> {
    let actual =
        actual.ok_or_else(|| LaunchError::Xdg(io::Error::other(format!("{name} is not set"))))?;
    if actual != expected {
        return Err(LaunchError::Xdg(io::Error::other(format!(
            "{name} is outside the canonical app directory"
        ))));
    }
    Ok(actual)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::os::unix::net::UnixListener;

    #[derive(Default)]
    struct FakeExec {
        executable: Option<PathBuf>,
        working_directory: Option<PathBuf>,
        environment: Vec<(String, String)>,
    }

    impl Exec for FakeExec {
        fn exec(
            &mut self,
            executable: &Path,
            working_directory: &Path,
            environment: &[(String, String)],
        ) -> io::Result<()> {
            self.executable = Some(executable.to_owned());
            self.working_directory = Some(working_directory.to_owned());
            self.environment = environment.to_vec();
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeEnforcer {
        policy: Option<PolicyDecision>,
    }

    impl PolicyEnforcer for FakeEnforcer {
        fn enforce_policy(
            &mut self,
            policy: &PolicyDecision,
        ) -> Result<AppliedPolicy, LandlockError> {
            self.policy = Some(policy.clone());
            Ok(AppliedPolicy {
                kernel_abi: 10,
                enforced: true,
                ruleset: None,
            })
        }
    }

    fn resolved_fixture(root: &Path, capabilities: &[&str]) -> pf_app_manifest::ResolvedApp {
        let capabilities = capabilities
            .iter()
            .map(|capability| format!("{capability:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let manifest = pf_app_manifest::parse_manifest(&format!(
            "[app]\nid = \"org.example.app\"\nuse = [{capabilities}]\n\
             [runtime]\nfamily = \"pocketforge/a133-powervr\"\nabi = \"1\"\n\
             platform-version = \"20\"\n[launch]\nexec = \"bin/app\"\n"
        ))
        .unwrap();
        let platform = pf_app_manifest::parse_platform_contract(&format!(
            "schema_version = 1\nruntime_family = \"pocketforge/a133-powervr\"\n\
             runtime_abi = \"1\"\nplatform_version = \"20\"\n\
             supported_capabilities = [{capabilities}]\nproduction = true\n\
             landlock_mode = \"required\"\nlandlock_full_min_abi = 10\n\
             landlock_browser_min_abi = 6\n"
        ))
        .unwrap();
        pf_app_manifest::ResolvedApp {
            id: "org.example.app".to_owned(),
            app_dir: root.join("app"),
            executable: root.join("app/bin/app"),
            manifest,
            manifest_sha256: "fixture".to_owned(),
            platform,
        }
    }

    #[test]
    fn device_regression_platform_library_is_in_read_execute_set() {
        let paths = platform_resource_paths();
        let read = |path: &Path| {
            paths
                .iter()
                .any(|root| path.starts_with(root))
                .then_some(())
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EACCES))
        };

        read(Path::new("/opt/pocketforge/lib/libSDL3-pocketforge.so.0")).unwrap();
        read(Path::new("/usr/lib/libc.so.6")).unwrap();
        for denied in ["/opt/foreign.so", "/opt/pocketforge/private/secret"] {
            assert_eq!(
                read(Path::new(denied)).unwrap_err().raw_os_error(),
                Some(libc::EACCES)
            );
        }
    }

    #[test]
    fn xdg_runtime_directory_must_be_the_private_unit_directory() {
        let expected = PathBuf::from("/run/pf-app/org.example.app");
        assert_eq!(
            validate_environment_path("XDG_RUNTIME_DIR", Some(expected.clone()), expected.clone())
                .unwrap(),
            expected
        );
        assert!(
            validate_environment_path("XDG_RUNTIME_DIR", None, expected.clone())
                .unwrap_err()
                .to_string()
                .contains("is not set")
        );
        assert!(validate_environment_path(
            "XDG_RUNTIME_DIR",
            Some(PathBuf::from("/run/pf-app/another.app")),
            expected
        )
        .unwrap_err()
        .to_string()
        .contains("outside the canonical app directory"));
    }

    #[test]
    fn x11_environment_is_validated_and_explicitly_scoped_to_exec() {
        let dir = std::env::temp_dir().join(format!(
            "pf-app-launch-x11-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("X0");
        let environment = dir.join("environment");
        let _listener = UnixListener::bind(&socket).unwrap();
        fs::write(&environment, b"DISPLAY=:0\n").unwrap();

        let root = PathBuf::from("/fixture");
        let app = resolved_fixture(&root, &["x11"]);
        let mut inputs = PolicyInputs {
            platform_runtime: root.join("runtime"),
            xdg_config: root.join("config"),
            xdg_state: root.join("state"),
            xdg_cache: root.join("cache"),
            resource_paths: Vec::new(),
            resources: std::collections::BTreeMap::from([
                (
                    "x11-socket".to_owned(),
                    vec![PathBuf::from(policy::X11_SOCKET)],
                ),
                (
                    "x11-environment".to_owned(),
                    vec![PathBuf::from(policy::X11_ENVIRONMENT)],
                ),
            ]),
        };
        let mut policy = derive_policy(&app, &inputs).unwrap();
        policy.unit.bind_read_write_mappings[0].destination = socket.clone();
        policy.environment[0].source = environment.clone();
        assert_eq!(
            load_environment(&policy).unwrap(),
            [("DISPLAY".to_owned(), ":0".to_owned())]
        );

        let socket_alias = dir.join("X0-alias");
        symlink(&socket, &socket_alias).unwrap();
        policy.unit.bind_read_write_mappings[0].destination = socket_alias;
        assert!(load_environment(&policy)
            .unwrap_err()
            .to_string()
            .contains("alias"));
        policy.unit.bind_read_write_mappings[0].destination = socket.clone();

        let environment_alias = dir.join("environment-alias");
        symlink(&environment, &environment_alias).unwrap();
        policy.environment[0].source = environment_alias;
        assert!(load_environment(&policy)
            .unwrap_err()
            .to_string()
            .contains("alias"));
        policy.environment[0].source = environment.clone();

        let parent_alias = dir.join("x11-parent-alias");
        symlink(&dir, &parent_alias).unwrap();
        policy.unit.bind_read_write_mappings[0].destination = parent_alias.join("X0");
        assert!(load_environment(&policy)
            .unwrap_err()
            .to_string()
            .contains("alias"));
        policy.unit.bind_read_write_mappings[0].destination = socket.clone();
        policy.environment[0].source = parent_alias.join("environment");
        assert!(load_environment(&policy)
            .unwrap_err()
            .to_string()
            .contains("alias"));
        policy.environment[0].source = environment.clone();

        let plain_command = execution_command(Path::new("/usr/bin/env"), &dir, &[]);
        assert_eq!(
            plain_command
                .get_envs()
                .find(|(name, _)| *name == "DISPLAY"),
            Some((std::ffi::OsStr::new("DISPLAY"), None))
        );
        for inherited in ["WAYLAND_DISPLAY", "XDG_RUNTIME_DIR"] {
            assert_eq!(
                plain_command
                    .get_envs()
                    .find(|(name, _)| *name == inherited),
                None,
                "{inherited} must remain inherited from the validated systemd unit"
            );
        }
        let x11_command = execution_command(
            Path::new("/usr/bin/env"),
            &dir,
            &[("DISPLAY".to_owned(), ":0".to_owned())],
        );
        assert_eq!(
            x11_command.get_envs().find(|(name, _)| *name == "DISPLAY"),
            Some((
                std::ffi::OsStr::new("DISPLAY"),
                Some(std::ffi::OsStr::new(":0"))
            ))
        );

        for malformed in [
            "DISPLAY=:1\n",
            "DISPLAY=@/tmp/.X11-unix/X0\n",
            "DISPLAY=:0\nLD_PRELOAD=/foreign.so\n",
            "DISPLAY=:0",
        ] {
            fs::write(&environment, malformed).unwrap();
            assert!(load_environment(&policy)
                .unwrap_err()
                .to_string()
                .contains("malformed"));
        }
        fs::remove_file(&environment).unwrap();
        assert!(load_environment(&policy)
            .unwrap_err()
            .to_string()
            .contains("read capability environment"));

        let abstract_name = dir.join("@X0");
        fs::write(&abstract_name, b"not a socket").unwrap();
        fs::write(&environment, b"DISPLAY=:0\n").unwrap();
        policy.unit.bind_read_write_mappings[0].destination = abstract_name;
        assert!(load_environment(&policy)
            .unwrap_err()
            .to_string()
            .contains("not a filesystem socket"));

        inputs.resources.clear();
        let plain = derive_policy(&resolved_fixture(&root, &[]), &inputs).unwrap();
        assert!(load_environment(&plain).unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn input_broker_environment_tracks_compiled_route_and_scrubs_ambient_value() {
        let root = PathBuf::from("/fixture");
        let app = resolved_fixture(&root, &["input"]);
        let paths = |resources| PolicyInputs {
            platform_runtime: root.join("runtime"),
            xdg_config: root.join("config"),
            xdg_state: root.join("state"),
            xdg_cache: root.join("cache"),
            resource_paths: Vec::new(),
            resources,
        };

        let raw = derive_policy(
            &app,
            &paths(std::collections::BTreeMap::from([(
                "input-devices".to_owned(),
                vec![PathBuf::from("/dev/input/event0")],
            )])),
        )
        .unwrap();
        let raw_environment = load_environment(&raw).unwrap();
        assert!(raw_environment
            .iter()
            .all(|(name, _)| name != "PF_BROKER_SOCK"));
        let raw_command = execution_command(Path::new("/usr/bin/env"), &root, &raw_environment);
        assert_eq!(
            raw_command
                .get_envs()
                .find(|(name, _)| *name == "PF_BROKER_SOCK"),
            Some((std::ffi::OsStr::new("PF_BROKER_SOCK"), None)),
            "a raw-input route must remove the unit's ambient broker selector"
        );

        let broker = derive_policy(
            &app,
            &paths(std::collections::BTreeMap::from([(
                "broker-sockets".to_owned(),
                vec![PathBuf::from(policy::INPUT_BROKER_SOCKET)],
            )])),
        )
        .unwrap();
        let broker_environment = load_environment(&broker).unwrap();
        assert!(broker_environment.iter().any(|(name, value)| {
            name == "PF_BROKER_SOCK" && value == policy::INPUT_BROKER_SOCKET
        }));
        let broker_command =
            execution_command(Path::new("/usr/bin/env"), &root, &broker_environment);
        assert_eq!(
            broker_command
                .get_envs()
                .find(|(name, _)| *name == "PF_BROKER_SOCK"),
            Some((
                std::ffi::OsStr::new("PF_BROKER_SOCK"),
                Some(std::ffi::OsStr::new(policy::INPUT_BROKER_SOCKET))
            )),
            "a broker route must restore the policy-derived broker selector"
        );

        let mismatched = derive_policy(
            &app,
            &paths(std::collections::BTreeMap::from([(
                "broker-sockets".to_owned(),
                vec![PathBuf::from("/run/pocketforge/not-input-broker.sock")],
            )])),
        )
        .unwrap();
        assert_eq!(
            load_environment(&mismatched).unwrap_err().to_string(),
            "input broker endpoint has no canonical socket projection"
        );
    }

    #[test]
    fn broker_only_capabilities_restore_canonical_selector_and_refuse_mismatch() {
        const BROKER_ONLY_CAPABILITIES: [&str; 7] = [
            "accelerometer",
            "gnss",
            "gyroscope",
            "imu",
            "leds",
            "location",
            "magnetometer",
        ];

        let root = PathBuf::from("/fixture");
        let paths = |socket: &str| PolicyInputs {
            platform_runtime: root.join("runtime"),
            xdg_config: root.join("config"),
            xdg_state: root.join("state"),
            xdg_cache: root.join("cache"),
            resource_paths: Vec::new(),
            resources: std::collections::BTreeMap::from([(
                "broker-sockets".to_owned(),
                vec![PathBuf::from(socket)],
            )]),
        };

        for mask in 1..(1 << BROKER_ONLY_CAPABILITIES.len()) {
            let capabilities = BROKER_ONLY_CAPABILITIES
                .iter()
                .enumerate()
                .filter_map(|(index, capability)| {
                    ((mask & (1 << index)) != 0).then_some(*capability)
                })
                .collect::<Vec<_>>();
            let app = resolved_fixture(&root, &capabilities);

            let canonical = derive_policy(&app, &paths(policy::INPUT_BROKER_SOCKET)).unwrap();
            assert!(
                canonical
                    .endpoints
                    .contains(&policy::MediatedEndpoint::InputBroker),
                "broker-only route must identify its endpoint for {capabilities:?}"
            );
            assert!(load_environment(&canonical)
                .unwrap()
                .iter()
                .any(|(name, value)| {
                    name == "PF_BROKER_SOCK" && value == policy::INPUT_BROKER_SOCKET
                }));

            let mismatched =
                derive_policy(&app, &paths("/run/pocketforge/not-input-broker.sock")).unwrap();
            assert_eq!(
                load_environment(&mismatched).unwrap_err().to_string(),
                "input broker endpoint has no canonical socket projection",
                "a noncanonical broker-only route must fail closed for {capabilities:?}"
            );
        }
    }

    #[test]
    fn x11_policy_uses_real_producer_paths_and_refuses_b15_session_alias() {
        assert_eq!(policy::X11_SOCKET, "/run/pocketforge/x11/.X11-unix/X0");
        assert_eq!(
            policy::X11_ENVIRONMENT,
            "/run/pocketforge/x11/policy/environment"
        );

        let dir = std::env::temp_dir().join(format!(
            "pf-app-launch-b15-x11-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let producer = dir.join("run/pocketforge/x11");
        fs::create_dir_all(producer.join(".X11-unix")).unwrap();
        let socket = producer.join(".X11-unix/X0");
        let environment = producer.join("environment");
        let _listener = UnixListener::bind(&socket).unwrap();
        fs::write(&environment, b"DISPLAY=:0\n").unwrap();

        let generation = dir.join("run/pocketforge/.session-generations/1");
        fs::create_dir_all(generation.join("capabilities/x11")).unwrap();
        symlink(&socket, generation.join("capabilities/x11/X0")).unwrap();
        symlink(
            &environment,
            generation.join("capabilities/x11/environment"),
        )
        .unwrap();
        let session = dir.join("run/pocketforge/session");
        symlink(&generation, &session).unwrap();

        let root = PathBuf::from("/fixture");
        let app = resolved_fixture(&root, &["x11"]);
        let inputs = PolicyInputs {
            platform_runtime: root.join("runtime"),
            xdg_config: root.join("config"),
            xdg_state: root.join("state"),
            xdg_cache: root.join("cache"),
            resource_paths: Vec::new(),
            resources: std::collections::BTreeMap::from([
                (
                    "x11-socket".to_owned(),
                    vec![PathBuf::from(policy::X11_SOCKET)],
                ),
                (
                    "x11-environment".to_owned(),
                    vec![PathBuf::from(policy::X11_ENVIRONMENT)],
                ),
            ]),
        };
        let mut policy = derive_policy(&app, &inputs).unwrap();

        // This is the exact Build 15 shape: the public session path has a symlink
        // parent and its X11 leaves are aliases. Strict descriptor traversal must
        // continue to reject it.
        policy.unit.bind_read_write_mappings[0].destination = session.join("capabilities/x11/X0");
        policy.environment[0].source = session.join("capabilities/x11/environment");
        assert!(load_environment(&policy)
            .unwrap_err()
            .to_string()
            .contains("alias"));

        policy.unit.bind_read_write_mappings[0].destination = socket;
        policy.environment[0].source = environment;
        assert_eq!(
            load_environment(&policy).unwrap(),
            [("DISPLAY".to_owned(), ":0".to_owned())]
        );

        let foreign = dir.join("foreign");
        fs::create_dir_all(&foreign).unwrap();
        let parent_alias = producer.join("foreign-parent");
        symlink(&foreign, &parent_alias).unwrap();
        policy.unit.bind_read_write_mappings[0].destination = parent_alias.join("X0");
        assert!(load_environment(&policy)
            .unwrap_err()
            .to_string()
            .contains("alias"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resolves_again_creates_xdg_directories_and_executes_without_arguments() {
        let dir = std::env::temp_dir().join(format!(
            "pf-app-launch-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("apps");
        let app = root.join("org.example.app");
        fs::create_dir_all(app.join("bin")).unwrap();
        fs::write(
            app.join("app.toml"),
            "[app]\nid = \"org.example.app\"\nuse = [\"audio\"]\n\
             [runtime]\nfamily = \"pocketforge/a133-powervr\"\nabi = \"1\"\nplatform-version = \"20\"\n\
             [launch]\nexec = \"bin/app\"\n",
        )
        .unwrap();
        let executable = app.join("bin/app");
        fs::write(&executable, b"#!/bin/sh\n").unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        let platform = dir.join("platform.toml");
        fs::write(
            &platform,
            "schema_version = 1\n\
             runtime_family = \"pocketforge/a133-powervr\"\n\
             runtime_abi = \"1\"\n\
             platform_version = \"20\"\n\
             supported_capabilities = [\"audio\"]\n\
             production = true\n\
             landlock_mode = \"required\"\n\
             landlock_full_min_abi = 10\n\
             landlock_browser_min_abi = 6\n",
        )
        .unwrap();
        let config = dir.join("state/config");
        let state = dir.join("state/state");
        let cache = dir.join("state/cache");
        let runtime = dir.join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        let mut exec = FakeExec::default();
        let mut enforcer = FakeEnforcer::default();

        launch_with(
            &Resolver::new(&root, &platform),
            "org.example.app",
            &LaunchPaths {
                xdg_config: config.clone(),
                xdg_state: state.clone(),
                xdg_cache: cache.clone(),
                platform_runtime: runtime,
                resource_paths: Vec::new(),
                resources: std::collections::BTreeMap::from([
                    (
                        "audio-devices".to_owned(),
                        vec![PathBuf::from("/dev/snd/controlC0")],
                    ),
                    (
                        "audio-sockets".to_owned(),
                        vec![PathBuf::from("/run/pocketforge/session/audio-0")],
                    ),
                    (
                        "session-environment".to_owned(),
                        vec![PathBuf::from("/run/pocketforge/session/environment")],
                    ),
                ]),
            },
            &mut enforcer,
            &mut exec,
        )
        .unwrap();

        assert!(config.is_dir());
        assert!(state.is_dir());
        assert!(cache.is_dir());
        assert_eq!(enforcer.policy.unwrap().app_id, "org.example.app");
        assert_eq!(exec.executable, Some(fs::canonicalize(executable).unwrap()));
        assert_eq!(exec.working_directory, Some(fs::canonicalize(app).unwrap()));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resolver_refusals_preserve_reason_and_exit_class() {
        let resolver = Resolver::new("/definitely/missing", "/also/missing");
        let mut exec = FakeExec::default();
        let mut enforcer = FakeEnforcer::default();
        let error = launch_with(
            &resolver,
            "../outside",
            &LaunchPaths {
                xdg_config: PathBuf::from("/unused/config"),
                xdg_state: PathBuf::from("/unused/state"),
                xdg_cache: PathBuf::from("/unused/cache"),
                platform_runtime: PathBuf::from("/unused/runtime"),
                resource_paths: Vec::new(),
                resources: std::collections::BTreeMap::new(),
            },
            &mut enforcer,
            &mut exec,
        )
        .unwrap_err();
        assert_eq!(error.reason(), ReasonCode::InvalidId);
        assert_eq!(error.exit_code(), 65);
        assert!(exec.executable.is_none());
    }
}
