//! Library seam for the `pf-app-launch <app-id>` helper.

pub mod landlock;
pub mod policy;

use crate::landlock::{apply_landlock, AppliedPolicy, LandlockError, LandlockKernel};
use crate::policy::{
    derive_policy, PolicyDecision, PolicyError, PolicyInputs, PolicyResourceInventory,
};
use pf_app_manifest::{ReasonCode, ResolveError, Resolver};
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub trait Exec {
    /// Replace the current process. A successful production call never returns.
    fn exec(&mut self, executable: &Path, working_directory: &Path) -> io::Result<()>;
}

#[derive(Default)]
pub struct ProcessExec;

impl Exec for ProcessExec {
    fn exec(&mut self, executable: &Path, working_directory: &Path) -> io::Result<()> {
        let error = Command::new(executable)
            .current_dir(working_directory)
            .exec();
        Err(error)
    }
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
    enforcer
        .enforce_policy(&policy)
        .map_err(LaunchError::Landlock)?;
    executor
        .exec(&resolved.executable, &resolved.app_dir)
        .map_err(LaunchError::Exec)
}

pub fn required_launch_paths(app_id: &str) -> Result<LaunchPaths, LaunchError> {
    let required = |name: &str, expected: PathBuf| {
        let actual = std::env::var_os(name)
            .map(PathBuf::from)
            .ok_or_else(|| LaunchError::Xdg(io::Error::other(format!("{name} is not set"))))?;
        if actual != expected {
            return Err(LaunchError::Xdg(io::Error::other(format!(
                "{name} is outside the canonical app directory"
            ))));
        }
        Ok(actual)
    };
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
    Ok(LaunchPaths {
        xdg_config: required("XDG_CONFIG_HOME", state_root.join("config"))?,
        xdg_state: required("XDG_STATE_HOME", state_root.join("state"))?,
        xdg_cache: required(
            "XDG_CACHE_HOME",
            PathBuf::from("/var/cache/pocketforge/apps").join(app_id),
        )?,
        platform_runtime: inventory.platform_runtime,
        resource_paths: platform_resource_paths(),
        resources: inventory.resources,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[derive(Default)]
    struct FakeExec {
        executable: Option<PathBuf>,
        working_directory: Option<PathBuf>,
    }

    impl Exec for FakeExec {
        fn exec(&mut self, executable: &Path, working_directory: &Path) -> io::Result<()> {
            self.executable = Some(executable.to_owned());
            self.working_directory = Some(working_directory.to_owned());
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
