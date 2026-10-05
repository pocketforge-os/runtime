//! Compositor-independent publication and app-root projection for a PocketForge session.
//!
//! The public session is a symlink to a complete, immutable generation.  A publisher writes
//! the generation first and atomically replaces the public symlink last.  Readers therefore
//! never consume a mixture of generations.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};

pub const DEFAULT_SESSION_ROOT: &str = "/run/pocketforge/session";
const GENERATIONS_DIR: &str = ".session-generations";
const READY: &str = "ready\n";
const READONLY_MODE: u32 = 0o444;
const DIRECTORY_MODE: u32 = 0o755;
const PROJECTION_DIRECTORY_MODE: u32 = 0o755;

/// A complete compositor publication.  Endpoint paths may be sockets or regular files in a
/// hermetic test; publication requires that they already exist and never takes ownership of
/// them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPublication {
    pub wayland_display: String,
    pub wayland_socket: PathBuf,
    pub xwayland: Option<XwaylandPublication>,
    pub capabilities: BTreeMap<String, PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct XwaylandPublication {
    pub display: String,
    pub xauthority: PathBuf,
}

impl SessionPublication {
    pub fn new(display: impl Into<String>, socket: impl Into<PathBuf>) -> Self {
        Self {
            wayland_display: display.into(),
            wayland_socket: socket.into(),
            xwayland: None,
            capabilities: BTreeMap::new(),
        }
    }

    pub fn with_xwayland(
        mut self,
        display: impl Into<String>,
        xauthority: impl Into<PathBuf>,
    ) -> Self {
        self.xwayland = Some(XwaylandPublication {
            display: display.into(),
            xauthority: xauthority.into(),
        });
        self
    }

    pub fn with_capability(
        mut self,
        name: impl Into<String>,
        endpoint: impl Into<PathBuf>,
    ) -> Result<Self, SessionContractError> {
        let name = name.into();
        validate_capability(&name)?;
        self.capabilities.insert(name, endpoint.into());
        Ok(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub root: PathBuf,
    pub generation: u64,
    pub wayland_display: String,
    pub wayland_socket: PathBuf,
    pub display: Option<String>,
    pub xauthority: Option<PathBuf>,
    pub capabilities: BTreeMap<String, PathBuf>,
}

#[derive(Debug)]
pub enum SessionContractError {
    Io {
        operation: String,
        path: PathBuf,
        source: io::Error,
    },
    InvalidPublication(String),
    NotReady {
        reason: String,
    },
    StaleGeneration {
        expected: u64,
        actual: u64,
    },
    DeniedCapability {
        name: String,
    },
    UnsafeExistingPath {
        path: PathBuf,
    },
}

impl fmt::Display for SessionContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => {
                write!(f, "{operation} {}: {source}", path.display())
            }
            Self::InvalidPublication(reason) => write!(f, "invalid session publication: {reason}"),
            Self::NotReady { reason } => write!(f, "session publication is not ready: {reason}"),
            Self::StaleGeneration { expected, actual } => {
                write!(
                    f,
                    "stale session generation: expected {expected}, got {actual}"
                )
            }
            Self::DeniedCapability { name } => write!(f, "capability is denied to apps: {name}"),
            Self::UnsafeExistingPath { path } => {
                write!(f, "unsafe existing path: {}", path.display())
            }
        }
    }
}

impl std::error::Error for SessionContractError {}

pub struct SessionPublisher {
    root: PathBuf,
}

impl SessionPublisher {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Publish a complete generation, returning its monotonically increasing generation number.
    pub fn publish(&self, publication: &SessionPublication) -> Result<u64, SessionContractError> {
        validate_publication(publication)?;
        let parent = self.parent_dir()?;
        ensure_directory(&parent, DIRECTORY_MODE)?;
        let generations = parent.join(GENERATIONS_DIR);
        ensure_directory(&generations, DIRECTORY_MODE)?;
        let generation = next_generation(&generations)?;
        let stage = generations.join(format!(".staging.{generation}.{}", std::process::id()));
        if stage.exists() {
            return Err(SessionContractError::UnsafeExistingPath { path: stage });
        }
        fs::create_dir(&stage).map_err(|source| io_error("create generation", &stage, source))?;
        set_mode(&stage, DIRECTORY_MODE)?;

        let result = self.write_generation(&stage, generation, publication);
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&stage);
            return Err(error);
        }

        let final_dir = generations.join(generation.to_string());
        fs::rename(&stage, &final_dir)
            .map_err(|source| io_error("commit generation", &final_dir, source))?;

        let temporary_link =
            parent.join(format!(".session-link.{generation}.{}", std::process::id()));
        symlink(
            Path::new(GENERATIONS_DIR).join(generation.to_string()),
            &temporary_link,
        )
        .map_err(|source| io_error("stage session link", &temporary_link, source))?;
        if let Err(error) = replace_public_link(&self.root, &temporary_link) {
            let _ = fs::remove_file(&temporary_link);
            return Err(error);
        }
        Ok(generation)
    }

    pub fn read(&self) -> Result<SessionSnapshot, SessionContractError> {
        let generation_root =
            fs::canonicalize(&self.root).map_err(|source| SessionContractError::NotReady {
                reason: format!("cannot resolve {}: {source}", self.root.display()),
            })?;
        let generations =
            fs::canonicalize(self.parent_dir()?.join(GENERATIONS_DIR)).map_err(|source| {
                SessionContractError::NotReady {
                    reason: format!("cannot resolve generation store: {source}"),
                }
            })?;
        if generation_root.parent() != Some(generations.as_path()) {
            return Err(SessionContractError::NotReady {
                reason: "public root does not point into the generation store".into(),
            });
        }

        let readiness = read_text(&generation_root.join("readiness"))?;
        if readiness != READY {
            return Err(SessionContractError::NotReady {
                reason: "readiness marker is not exactly ready\\n".into(),
            });
        }
        let generation = parse_generation(&read_text(&generation_root.join("generation"))?)?;
        let environment = parse_environment(&read_text(&generation_root.join("environment"))?)?;
        let expected_root = self.root.to_string_lossy();
        if environment.get("POCKETFORGE_SESSION").map(String::as_str)
            != Some(expected_root.as_ref())
        {
            return Err(SessionContractError::NotReady {
                reason: "environment names a different session root".into(),
            });
        }
        if environment
            .get("POCKETFORGE_SESSION_GENERATION")
            .map(String::as_str)
            != Some(generation.to_string().as_str())
        {
            return Err(SessionContractError::NotReady {
                reason: "environment generation does not match generation file".into(),
            });
        }
        let wayland_display = environment
            .get("WAYLAND_DISPLAY")
            .cloned()
            .ok_or_else(|| not_ready("WAYLAND_DISPLAY is missing"))?;
        validate_component(&wayland_display, "WAYLAND_DISPLAY")?;
        let wayland_socket =
            existing_target(&generation_root.join(&wayland_display), "Wayland socket")?;

        let display = environment.get("DISPLAY").cloned();
        let xauthority = match environment.get("XAUTHORITY") {
            Some(path) => {
                let expected = self.root.join("xauthority");
                if path != &expected.to_string_lossy() {
                    return Err(not_ready(
                        "XAUTHORITY does not name the canonical projection",
                    ));
                }
                Some(existing_target(
                    &generation_root.join("xauthority"),
                    "Xauthority",
                )?)
            }
            None => None,
        };
        if display.is_some() != xauthority.is_some() {
            return Err(not_ready(
                "DISPLAY and XAUTHORITY must be published together",
            ));
        }

        let mut capabilities = BTreeMap::new();
        let capability_dir = generation_root.join("capabilities");
        if capability_dir.exists() {
            for entry in fs::read_dir(&capability_dir)
                .map_err(|source| io_error("read capabilities", &capability_dir, source))?
            {
                let entry = entry
                    .map_err(|source| io_error("read capability entry", &capability_dir, source))?;
                let name = entry.file_name().to_string_lossy().into_owned();
                validate_capability(&name)?;
                capabilities.insert(name, existing_target(&entry.path(), "capability endpoint")?);
            }
        }

        Ok(SessionSnapshot {
            root: self.root.clone(),
            generation,
            wayland_display,
            wayland_socket,
            display,
            xauthority,
            capabilities,
        })
    }

    pub fn reconnect(
        &self,
        expected_generation: u64,
    ) -> Result<SessionSnapshot, SessionContractError> {
        let snapshot = self.read()?;
        if snapshot.generation != expected_generation {
            return Err(SessionContractError::StaleGeneration {
                expected: expected_generation,
                actual: snapshot.generation,
            });
        }
        Ok(snapshot)
    }

    /// Project only compositor/session metadata and explicitly requested non-privileged endpoints
    /// into an app root.  The projection never contains DRM, input-owner, evdev, or uinput access.
    pub fn project_app_root(
        &self,
        app_root: &Path,
        capabilities: &[&str],
    ) -> Result<(), SessionContractError> {
        let snapshot = self.read()?;
        for name in capabilities {
            validate_capability(name)?;
            if !snapshot.capabilities.contains_key(*name) {
                return Err(SessionContractError::InvalidPublication(format!(
                    "capability is not published: {name}"
                )));
            }
        }
        ensure_directory(app_root, PROJECTION_DIRECTORY_MODE)?;
        let run = app_root.join("run");
        ensure_directory(&run, PROJECTION_DIRECTORY_MODE)?;
        let pocketforge = run.join("pocketforge");
        ensure_directory(&pocketforge, PROJECTION_DIRECTORY_MODE)?;
        let projection = pocketforge.join("session");
        ensure_directory(&projection, PROJECTION_DIRECTORY_MODE)?;

        write_readonly(
            &projection.join("generation"),
            &format!("{}\n", snapshot.generation),
        )?;
        write_readonly(&projection.join("readiness"), READY)?;
        let mut environment = format!(
            "POCKETFORGE_SESSION={}\nPOCKETFORGE_SESSION_GENERATION={}\nWAYLAND_DISPLAY={}\n",
            projection.display(),
            snapshot.generation,
            snapshot.wayland_display
        );
        if let Some(display) = &snapshot.display {
            environment.push_str(&format!(
                "DISPLAY={display}\nXAUTHORITY={}/xauthority\n",
                projection.display()
            ));
        }
        write_readonly(&projection.join("environment"), &environment)?;
        link_entry(
            &self.root.join(&snapshot.wayland_display),
            &projection.join(&snapshot.wayland_display),
        )?;
        if snapshot.xauthority.is_some() {
            link_entry(
                &self.root.join("xauthority"),
                &projection.join("xauthority"),
            )?;
        }
        let capability_dir = projection.join("capabilities");
        ensure_directory(&capability_dir, PROJECTION_DIRECTORY_MODE)?;
        for name in capabilities {
            link_entry(
                &self.root.join("capabilities").join(name),
                &capability_dir.join(name),
            )?;
        }
        Ok(())
    }

    fn parent_dir(&self) -> Result<PathBuf, SessionContractError> {
        self.root.parent().map(Path::to_path_buf).ok_or_else(|| {
            SessionContractError::InvalidPublication("session root has no parent".into())
        })
    }

    fn write_generation(
        &self,
        generation_root: &Path,
        generation: u64,
        publication: &SessionPublication,
    ) -> Result<(), SessionContractError> {
        let root_string = self.root.to_string_lossy();
        let mut environment = format!(
            "POCKETFORGE_SESSION={root_string}\nPOCKETFORGE_SESSION_GENERATION={generation}\nWAYLAND_DISPLAY={}\n",
            publication.wayland_display
        );
        if let Some(xwayland) = &publication.xwayland {
            environment.push_str(&format!(
                "DISPLAY={}\nXAUTHORITY={}/xauthority\n",
                xwayland.display, root_string
            ));
        }
        write_readonly(
            &generation_root.join("generation"),
            &format!("{generation}\n"),
        )?;
        write_readonly(&generation_root.join("environment"), &environment)?;
        link_entry_absolute(
            &publication.wayland_socket,
            &generation_root.join(&publication.wayland_display),
        )?;
        if let Some(xwayland) = &publication.xwayland {
            link_entry_absolute(&xwayland.xauthority, &generation_root.join("xauthority"))?;
        }
        let capabilities = generation_root.join("capabilities");
        ensure_directory(&capabilities, DIRECTORY_MODE)?;
        for (name, endpoint) in &publication.capabilities {
            link_entry_absolute(endpoint, &capabilities.join(name))?;
        }
        // Readiness is deliberately the final file in the staged generation.
        write_readonly(&generation_root.join("readiness"), READY)
    }
}

fn validate_publication(publication: &SessionPublication) -> Result<(), SessionContractError> {
    validate_component(&publication.wayland_display, "WAYLAND_DISPLAY")?;
    if !publication.wayland_display.starts_with("wayland-") {
        return Err(invalid("WAYLAND_DISPLAY must name a wayland-* socket"));
    }
    validate_endpoint(&publication.wayland_socket, "Wayland socket")?;
    if let Some(xwayland) = &publication.xwayland {
        if xwayland.display.is_empty() || xwayland.display.contains(['\n', '\r', '=']) {
            return Err(invalid("DISPLAY contains an invalid value"));
        }
        validate_endpoint(&xwayland.xauthority, "Xauthority")?;
    }
    for (name, endpoint) in &publication.capabilities {
        validate_capability(name)?;
        validate_endpoint(endpoint, "capability endpoint")?;
    }
    Ok(())
}

fn validate_endpoint(path: &Path, label: &str) -> Result<(), SessionContractError> {
    fs::metadata(path)
        .map(|_| ())
        .map_err(|source| io_error(label, path, source))
}

fn validate_component(value: &str, label: &str) -> Result<(), SessionContractError> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains(['\n', '\r', '='])
    {
        return Err(invalid(format!(
            "{label} contains an unsafe path component"
        )));
    }
    Ok(())
}

fn validate_capability(name: &str) -> Result<(), SessionContractError> {
    validate_component(name, "capability")?;
    if matches!(
        name,
        "drm"
            | "drm-master"
            | "drm-control"
            | "drm-render"
            | "drm_render"
            | "protected-input"
            | "protected_input"
            | "input-ownership"
            | "input_owner"
            | "evdev"
            | "uinput"
    ) {
        return Err(SessionContractError::DeniedCapability { name: name.into() });
    }
    Ok(())
}

fn existing_target(path: &Path, label: &str) -> Result<PathBuf, SessionContractError> {
    fs::canonicalize(path).map_err(|source| io_error(label, path, source))
}

fn next_generation(generations: &Path) -> Result<u64, SessionContractError> {
    let mut maximum = 0;
    for entry in fs::read_dir(generations)
        .map_err(|source| io_error("read generations", generations, source))?
    {
        let entry =
            entry.map_err(|source| io_error("read generation entry", generations, source))?;
        if let Ok(value) = entry.file_name().to_string_lossy().parse::<u64>() {
            maximum = maximum.max(value);
        }
    }
    maximum
        .checked_add(1)
        .ok_or_else(|| invalid("generation counter overflow"))
}

fn parse_generation(text: &str) -> Result<u64, SessionContractError> {
    text.trim_end_matches('\n')
        .parse()
        .map_err(|_| not_ready("generation is not a decimal number"))
}

fn parse_environment(text: &str) -> Result<BTreeMap<String, String>, SessionContractError> {
    let mut values = BTreeMap::new();
    for line in text.lines() {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| not_ready("environment contains a malformed line"))?;
        if key.is_empty() || values.insert(key.to_owned(), value.to_owned()).is_some() {
            return Err(not_ready("environment contains a duplicate or empty key"));
        }
    }
    Ok(values)
}

fn ensure_directory(path: &Path, mode: u32) -> Result<(), SessionContractError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(SessionContractError::UnsafeExistingPath { path: path.into() }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|source| io_error("create directory", path, source))?;
            set_mode(path, mode)
        }
        Err(source) => Err(io_error("inspect directory", path, source)),
    }
}

fn write_readonly(path: &Path, contents: &str) -> Result<(), SessionContractError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| io_error("create contract file", path, source))?;
    file.write_all(contents.as_bytes())
        .map_err(|source| io_error("write contract file", path, source))?;
    file.sync_all()
        .map_err(|source| io_error("sync contract file", path, source))?;
    drop(file);
    set_mode(path, READONLY_MODE)
}

fn link_entry_absolute(source: &Path, destination: &Path) -> Result<(), SessionContractError> {
    let source = absolute_path(source);
    link_entry(&source, destination)
}

fn link_entry(source: &Path, destination: &Path) -> Result<(), SessionContractError> {
    if destination.exists() || fs::symlink_metadata(destination).is_ok() {
        if fs::symlink_metadata(destination)
            .map_err(|source| io_error("inspect link", destination, source))?
            .file_type()
            .is_symlink()
        {
            fs::remove_file(destination)
                .map_err(|source| io_error("replace link", destination, source))?;
        } else {
            return Err(SessionContractError::UnsafeExistingPath {
                path: destination.into(),
            });
        }
    }
    symlink(source, destination).map_err(|source| io_error("create link", destination, source))
}

fn replace_public_link(root: &Path, temporary_link: &Path) -> Result<(), SessionContractError> {
    match fs::symlink_metadata(root) {
        Ok(metadata) if !metadata.file_type().is_symlink() => {
            Err(SessionContractError::UnsafeExistingPath { path: root.into() })
        }
        Ok(_) => fs::rename(temporary_link, root)
            .map_err(|source| io_error("replace session link", root, source)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::rename(temporary_link, root)
            .map_err(|source| io_error("publish session link", root, source)),
        Err(source) => Err(io_error("inspect session link", root, source)),
    }
}

fn read_text(path: &Path) -> Result<String, SessionContractError> {
    fs::read_to_string(path)
        .map_err(|source| not_ready(format!("{} is unavailable: {source}", path.display())))
}

fn set_mode(path: &Path, mode: u32) -> Result<(), SessionContractError> {
    let mut permissions = fs::metadata(path)
        .map_err(|source| io_error("inspect permissions", path, source))?
        .permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions)
        .map_err(|source| io_error("set permissions", path, source))
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    }
}

fn invalid(reason: impl Into<String>) -> SessionContractError {
    SessionContractError::InvalidPublication(reason.into())
}

fn not_ready(reason: impl Into<String>) -> SessionContractError {
    SessionContractError::NotReady {
        reason: reason.into(),
    }
}

fn io_error(operation: impl Into<String>, path: &Path, source: io::Error) -> SessionContractError {
    SessionContractError::Io {
        operation: operation.into(),
        path: path.into(),
        source,
    }
}
