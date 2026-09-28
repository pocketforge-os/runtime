//! Library seam for the `pf-app-launch <app-id>` helper.

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
    Exec(io::Error),
}

impl LaunchError {
    pub const fn reason(&self) -> ReasonCode {
        match self {
            Self::Resolve(error) => error.reason,
            Self::Xdg(_) | Self::Exec(_) => ReasonCode::ExecIo,
        }
    }

    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::Resolve(error) => error.helper_exit_code(),
            Self::Xdg(_) => 74,
            Self::Exec(_) => 126,
        }
    }
}

impl fmt::Display for LaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resolve(error) => error.fmt(f),
            Self::Xdg(error) => write!(f, "XDG directory: {error}"),
            Self::Exec(error) => write!(f, "execve: {error}"),
        }
    }
}

impl std::error::Error for LaunchError {}

/// Resolve, prepare the authorized XDG directories, and execute an installed application.
///
/// Production uses [`Resolver::fixed`]; explicit roots and an executor are accepted here solely
/// for hermetic tests.
pub fn launch_with<E: Exec>(
    resolver: &Resolver,
    app_id: &str,
    xdg_config_home: &Path,
    xdg_state_home: &Path,
    executor: &mut E,
) -> Result<(), LaunchError> {
    let resolved = resolver.resolve(app_id).map_err(LaunchError::Resolve)?;
    for directory in [xdg_config_home, xdg_state_home] {
        fs::create_dir_all(directory).map_err(LaunchError::Xdg)?;
    }
    executor
        .exec(&resolved.executable, &resolved.app_dir)
        .map_err(LaunchError::Exec)
}

pub fn required_xdg_paths() -> Result<(PathBuf, PathBuf), LaunchError> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .ok_or_else(|| LaunchError::Xdg(io::Error::other("XDG_CONFIG_HOME is not set")))?;
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .ok_or_else(|| LaunchError::Xdg(io::Error::other("XDG_STATE_HOME is not set")))?;
    Ok((config, state))
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
             supported_capabilities = [\"audio\"]\n",
        )
        .unwrap();
        let config = dir.join("state/config");
        let state = dir.join("state/state");
        let mut exec = FakeExec::default();

        launch_with(
            &Resolver::new(&root, &platform),
            "org.example.app",
            &config,
            &state,
            &mut exec,
        )
        .unwrap();

        assert!(config.is_dir());
        assert!(state.is_dir());
        assert_eq!(exec.executable, Some(fs::canonicalize(executable).unwrap()));
        assert_eq!(exec.working_directory, Some(fs::canonicalize(app).unwrap()));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resolver_refusals_preserve_reason_and_exit_class() {
        let resolver = Resolver::new("/definitely/missing", "/also/missing");
        let mut exec = FakeExec::default();
        let error = launch_with(
            &resolver,
            "../outside",
            Path::new("/unused/config"),
            Path::new("/unused/state"),
            &mut exec,
        )
        .unwrap_err();
        assert_eq!(error.reason(), ReasonCode::InvalidId);
        assert_eq!(error.exit_code(), 65);
        assert!(exec.executable.is_none());
    }
}
