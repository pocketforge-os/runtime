//! Linux adapters for the daemon-owned orientation reducer.
//!
//! The adapter deliberately samples stateful inputs as complete snapshots: connector/DT mount,
//! current EV_SW bits, iio-sensor-proxy's classified orientation, preferences, and the foreground
//! app manifest. A panel orientation that is missing, unreadable, invalid, or late uses a typed
//! descriptor/unknown fallback and marks the v1 state degraded instead of blocking daemon
//! readiness. Re-sampling on every source interval recovers that fallback, resume state, and lost
//! evdev reports without depending on an edge arriving.

use crate::orientation::{
    HallDebouncer, LogicalOrientation, MountMatrix, OrientationInputSource, OrientationInputs,
    PanelOrientationFailure, PanelOrientationSource, QuarterTurn, SensorObservation,
    AUTO_ROTATE_KEY, USER_ROTATION_KEY,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

const EV_SW: u16 = 0x05;
const SW_LID: u16 = 0x00;
const SW_TABLET_MODE: u16 = 0x01;
const SW_ROTATE_LOCK: u16 = 0x0c;

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum FixtureSwitch {
    Value(bool),
    Error { error: String },
}

#[derive(Clone, Debug)]
pub struct LinuxOrientationPaths {
    pub descriptor: PathBuf,
    pub prefs_socket: PathBuf,
    pub app_root: PathBuf,
    pub drm_class: PathBuf,
    pub device_tree: PathBuf,
    pub input_class: PathBuf,
    pub input_devices: PathBuf,
    pub iio_devices: PathBuf,
    /// Hermetic stand-in for the SensorProxy property. Production leaves this `None` and uses
    /// `busctl`; tests point it at an ordinary text file.
    pub sensor_orientation_file: Option<PathBuf>,
    /// Hermetic stand-in for EVIOCGSW, keyed by evdev name and Linux SW_* token.
    pub switch_state_file: Option<PathBuf>,
}

impl Default for LinuxOrientationPaths {
    fn default() -> Self {
        Self {
            descriptor: PathBuf::from("/run/pocketforge/missing-device-capabilities.toml"),
            prefs_socket: PathBuf::from("/run/pocketforge/prefsd.sock"),
            app_root: PathBuf::from(pf_app_manifest::APP_ROOT),
            drm_class: PathBuf::from("/sys/class/drm"),
            device_tree: PathBuf::from("/proc/device-tree"),
            input_class: PathBuf::from("/sys/class/input"),
            input_devices: PathBuf::from("/dev/input"),
            iio_devices: PathBuf::from("/sys/bus/iio/devices"),
            sensor_orientation_file: None,
            switch_state_file: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Descriptor {
    screens: Vec<Screen>,
    #[serde(default)]
    sensors: Vec<Sensor>,
    #[serde(default)]
    joints: Vec<Joint>,
}

#[derive(Debug, Deserialize)]
struct Screen {
    #[serde(default)]
    role: String,
    #[serde(default)]
    rotation: String,
}

#[derive(Debug, Deserialize)]
struct Sensor {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    iio_device: Option<String>,
    #[serde(default)]
    mount_matrix: Option<Vec<Vec<i8>>>,
}

#[derive(Debug, Deserialize)]
struct Joint {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    default: Option<f64>,
    #[serde(default)]
    switches: Vec<JointSwitch>,
    #[serde(default)]
    postures: Vec<Posture>,
}

#[derive(Debug, Deserialize)]
struct JointSwitch {
    code: String,
    source: String,
    active: [f64; 2],
}

#[derive(Debug, Deserialize)]
struct Posture {
    range: [f64; 2],
    #[serde(default)]
    screen_rotation: BTreeMap<String, String>,
}

pub struct LinuxOrientationSource {
    paths: LinuxOrientationPaths,
    descriptor: Descriptor,
    hall: Option<HallDebouncer>,
    panel_source: Option<PanelOrientationSource>,
    started: Instant,
}

impl LinuxOrientationSource {
    pub fn open(paths: LinuxOrientationPaths) -> Result<Self, String> {
        let text = fs::read_to_string(&paths.descriptor)
            .map_err(|error| format!("read {}: {error}", paths.descriptor.display()))?;
        let descriptor: Descriptor = toml::from_str(&text)
            .map_err(|error| format!("parse {}: {error}", paths.descriptor.display()))?;
        if descriptor.screens.is_empty() {
            return Err(format!(
                "parse {}: device descriptor has no screens",
                paths.descriptor.display()
            ));
        }
        Ok(Self {
            paths,
            descriptor,
            hall: None,
            panel_source: None,
            started: Instant::now(),
        })
    }

    fn expected_panel(&self) -> Option<QuarterTurn> {
        self.descriptor
            .screens
            .iter()
            .find(|screen| screen.role == "primary")
            .or_else(|| self.descriptor.screens.first())
            .and_then(|screen| parse_rotation(&screen.rotation))
    }

    fn panel_orientation(
        &mut self,
        descriptor: Option<QuarterTurn>,
    ) -> (QuarterTurn, PanelOrientationSource) {
        let drm = probe_drm_panel_orientation(&self.paths.drm_class);
        let (orientation, source) = if let Some(orientation) = drm.orientation {
            (orientation, PanelOrientationSource::DrmConnector)
        } else {
            let dt = probe_dt_rotation(&self.paths.device_tree);
            if let Some(orientation) = dt.orientation {
                (orientation, PanelOrientationSource::DeviceTree)
            } else {
                let failure = drm.failure.combine(dt.failure);
                match descriptor {
                    Some(orientation) => (
                        orientation,
                        PanelOrientationSource::DescriptorFallback(failure),
                    ),
                    None => (
                        QuarterTurn::Normal,
                        PanelOrientationSource::UnknownFallback(failure),
                    ),
                }
            }
        };
        if self.panel_source != Some(source) {
            match source {
                PanelOrientationSource::DescriptorFallback(reason) => eprintln!(
                    "pf-session-authorityd: panel_orientation_fallback reason={} fallback=descriptor",
                    reason.as_str()
                ),
                PanelOrientationSource::UnknownFallback(reason) => eprintln!(
                    "pf-session-authorityd: panel_orientation_fallback reason={} fallback=unknown",
                    reason.as_str()
                ),
                PanelOrientationSource::DrmConnector | PanelOrientationSource::DeviceTree
                    if self.panel_source.is_some_and(PanelOrientationSource::is_fallback) =>
                {
                    eprintln!(
                        "pf-session-authorityd: panel_orientation_recovered source={}",
                        match source {
                            PanelOrientationSource::DrmConnector => "drm_connector",
                            _ => "device_tree",
                        }
                    );
                }
                _ => {}
            }
            self.panel_source = Some(source);
        }
        (orientation, source)
    }

    fn preferences(&self, inputs: &mut OrientationInputs) -> Result<(), String> {
        let client = pf_prefsd::Client::new(&self.paths.prefs_socket);
        inputs.auto_rotate = client
            .get(AUTO_ROTATE_KEY)
            .map_err(|error| error.to_string())?
            .as_bool()
            .ok_or_else(|| "auto_rotate is not a bool".to_owned())?;
        let rotation = client
            .get(USER_ROTATION_KEY)
            .map_err(|error| error.to_string())?;
        inputs.user_rotation = rotation
            .as_str()
            .and_then(parse_rotation)
            .ok_or_else(|| "user_rotation is not a known rotation".to_owned())?;
        Ok(())
    }

    fn app_orientations(&self, app: Option<&str>) -> Result<Vec<LogicalOrientation>, String> {
        let Some(app) = app else {
            return Ok(vec![LogicalOrientation::Landscape]);
        };
        pf_app_manifest::validate_app_id(app).map_err(|_| "invalid active app id".to_owned())?;
        let path = self.paths.app_root.join(app).join("app.toml");
        let text = fs::read_to_string(&path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        let manifest = pf_app_manifest::parse_manifest(&text)
            .map_err(|error| format!("parse {}: {error}", path.display()))?;
        let Some(display) = manifest.display else {
            return Ok(vec![LogicalOrientation::Landscape]);
        };
        Ok(display
            .orientations
            .into_iter()
            .map(|orientation| match orientation {
                pf_app_manifest::DisplayOrientation::Landscape => LogicalOrientation::Landscape,
                pf_app_manifest::DisplayOrientation::LandscapeFlipped => {
                    LogicalOrientation::LandscapeFlipped
                }
                pf_app_manifest::DisplayOrientation::Portrait => LogicalOrientation::Portrait,
                pf_app_manifest::DisplayOrientation::PortraitFlipped => {
                    LogicalOrientation::PortraitFlipped
                }
            })
            .collect())
    }

    fn accelerometer(&self, inputs: &mut OrientationInputs) {
        let sensor = self.descriptor.sensors.iter().find(|sensor| {
            let kind = sensor.kind.to_ascii_lowercase();
            kind.contains("accel") || kind == "imu"
        });
        inputs.accelerometer_present = sensor.is_some();
        let Some(sensor) = sensor else {
            inputs.accelerometer = None;
            inputs.accelerometer_mount_matches = true;
            return;
        };
        inputs.accelerometer = read_sensor_orientation(&self.paths);
        const IDENTITY: MountMatrix = [[1, 0, 0], [0, 1, 0], [0, 0, 1]];
        let descriptor_matrix = descriptor_matrix(sensor).unwrap_or(IDENTITY);
        let kernel_matrix = kernel_matrix(sensor, &self.paths.iio_devices).unwrap_or(IDENTITY);
        inputs.accelerometer_mount_matches = descriptor_matrix == kernel_matrix;
    }

    fn switches(&mut self, inputs: &mut OrientationInputs) -> Result<(), String> {
        let fixture: BTreeMap<String, BTreeMap<String, FixtureSwitch>> =
            match &self.paths.switch_state_file {
                Some(path) => serde_json::from_slice(
                    &fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?,
                )
                .map_err(|error| format!("parse {}: {error}", path.display()))?,
                None => BTreeMap::new(),
            };
        let devices = if self.paths.switch_state_file.is_some() {
            Vec::new()
        } else {
            discover_switch_devices(&self.paths.input_class, &self.paths.input_devices)
                .map_err(|error| format!("discover evdev switches: {error}"))?
        };
        let fixture_value = |source: &str, token: &str| -> Result<Option<bool>, String> {
            match fixture.get(source).and_then(|values| values.get(token)) {
                Some(FixtureSwitch::Value(value)) => Ok(Some(*value)),
                Some(FixtureSwitch::Error { error }) => {
                    Err(format!("read {token} from {source}: {error}"))
                }
                None => Ok(None),
            }
        };
        let fixture_values = |token: &str| -> Result<Vec<bool>, String> {
            fixture
                .keys()
                .map(|source| fixture_value(source, token))
                .collect::<Result<Vec<_>, _>>()
                .map(|values| values.into_iter().flatten().collect())
        };
        inputs.rotate_locks = if self.paths.switch_state_file.is_some() {
            fixture_values("SW_ROTATE_LOCK")?
        } else {
            devices
                .iter()
                .map(|device| {
                    device.switch(SW_ROTATE_LOCK).map_err(|error| {
                        format!("read SW_ROTATE_LOCK from {}: {error}", device.name)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .flatten()
                .collect()
        };
        inputs.tablet_mode = if self.paths.switch_state_file.is_some() {
            fixture_values("SW_TABLET_MODE")?.into_iter().next()
        } else {
            let values = devices
                .iter()
                .map(|device| {
                    device.switch(SW_TABLET_MODE).map_err(|error| {
                        format!("read SW_TABLET_MODE from {}: {error}", device.name)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            values.into_iter().flatten().next()
        };
        inputs.tablet_mode_is_posture = false;

        let Some(joint) = self
            .descriptor
            .joints
            .iter()
            .find(|joint| joint.kind == "swivel" || joint.kind == "hinge")
        else {
            inputs.screen_posture = QuarterTurn::Normal;
            return Ok(());
        };
        let default_value = joint.default.unwrap_or_else(|| {
            joint
                .postures
                .first()
                .map(|posture| posture.range[0])
                .unwrap_or(0.0)
        });
        let mut value = default_value;
        for switch in &joint.switches {
            let Some(code) = parse_switch_code(&switch.code) else {
                continue;
            };
            let set = if self.paths.switch_state_file.is_some() {
                fixture_value(&switch.source, &switch.code)?
            } else {
                let device = devices
                    .iter()
                    .find(|device| device.name == switch.source)
                    .ok_or_else(|| format!("required evdev source {} is absent", switch.source))?;
                device.switch(code).map_err(|error| {
                    format!("read {} from {}: {error}", switch.code, switch.source)
                })?
            };
            let set = set.ok_or_else(|| {
                format!(
                    "required switch {} is unsupported by {}",
                    switch.code, switch.source
                )
            })?;
            if code == SW_TABLET_MODE {
                inputs.tablet_mode_is_posture = true;
            }
            if set {
                value = (switch.active[0] + switch.active[1]) / 2.0;
            }
        }
        let proposed = posture_rotation(joint, value);
        let now_ms = self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let hall = self
            .hall
            .get_or_insert_with(|| HallDebouncer::new(proposed, 50));
        hall.observe(proposed, now_ms);
        inputs.screen_posture = hall.advance(now_ms);
        Ok(())
    }
}

/// Resolve the canonical per-device descriptor using the runtime's device-selection contract.
pub fn production_descriptor_path() -> Result<PathBuf, String> {
    resolve_descriptor_path(
        std::env::var_os("PF_DESCRIPTOR"),
        std::env::var_os("PF_DEVICE_ID"),
        std::env::var_os("PF_PLATFORM"),
        Path::new("/usr/share/pocketforge/devices"),
    )
}

fn resolve_descriptor_path(
    explicit: Option<std::ffi::OsString>,
    device_id: Option<std::ffi::OsString>,
    platform: Option<std::ffi::OsString>,
    installed_devices: &Path,
) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return Ok(path.into());
    }
    match (device_id, platform) {
        (Some(id), Some(root)) => {
            return Ok(PathBuf::from(root)
                .join("devices")
                .join(id)
                .join("capabilities.toml"));
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err("PF_DEVICE_ID and PF_PLATFORM must be set together".to_owned());
        }
        (None, None) => {}
    }
    let mut candidates = fs::read_dir(installed_devices)
        .map_err(|error| format!("read {}: {error}", installed_devices.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read {}: {error}", installed_devices.display()))?
        .into_iter()
        .map(|entry| entry.path().join("capabilities.toml"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    candidates.sort();
    match candidates.as_slice() {
        [only] => Ok(only.clone()),
        [] => Err(format!(
            "no installed device descriptor beneath {}",
            installed_devices.display()
        )),
        _ => Err(format!(
            "multiple installed device descriptors beneath {}; set PF_DESCRIPTOR",
            installed_devices.display()
        )),
    }
}

impl OrientationInputSource for LinuxOrientationSource {
    fn snapshot(
        &mut self,
        active_app: Option<&str>,
        previous: &OrientationInputs,
    ) -> Result<OrientationInputs, String> {
        let mut inputs = previous.clone();
        let descriptor = self.expected_panel();
        inputs.descriptor_panel_orientation = descriptor;
        (inputs.panel_orientation, inputs.panel_orientation_source) =
            self.panel_orientation(descriptor);
        self.preferences(&mut inputs)?;
        inputs.app_orientations = self.app_orientations(active_app)?;
        self.accelerometer(&mut inputs);
        self.switches(&mut inputs)?;
        Ok(inputs)
    }
}

fn parse_rotation(value: &str) -> Option<QuarterTurn> {
    match value.trim().to_ascii_lowercase().as_str() {
        "normal" | "none" | "cw0" | "0" => Some(QuarterTurn::Normal),
        "left" | "left side up" | "left-side-up" | "cw270" | "270" => Some(QuarterTurn::Left),
        "upsidedown" | "upside_down" | "bottom up" | "bottom-up" | "cw180" | "180" => {
            Some(QuarterTurn::UpsideDown)
        }
        "right" | "right side up" | "right-side-up" | "cw90" | "90" => Some(QuarterTurn::Right),
        _ => None,
    }
}

fn posture_rotation(joint: &Joint, value: f64) -> QuarterTurn {
    joint
        .postures
        .iter()
        .find(|posture| value >= posture.range[0] && value <= posture.range[1])
        .and_then(|posture| posture.screen_rotation.values().next())
        .and_then(|rotation| parse_rotation(rotation))
        .unwrap_or(QuarterTurn::Normal)
}

impl PanelOrientationFailure {
    const fn combine(self, other: Self) -> Self {
        use PanelOrientationFailure::{Invalid, Missing, Unreadable};
        match (self, other) {
            (Unreadable, _) | (_, Unreadable) => Unreadable,
            (Invalid, _) | (_, Invalid) => Invalid,
            _ => Missing,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Unreadable => "unreadable",
            Self::Invalid => "invalid",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PanelProbe {
    orientation: Option<QuarterTurn>,
    failure: PanelOrientationFailure,
}

fn io_failure(error: &io::Error) -> PanelOrientationFailure {
    if error.kind() == io::ErrorKind::NotFound {
        PanelOrientationFailure::Missing
    } else {
        PanelOrientationFailure::Unreadable
    }
}

fn probe_drm_panel_orientation(root: &Path) -> PanelProbe {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            return PanelProbe {
                orientation: None,
                failure: io_failure(&error),
            };
        }
    };
    let mut failure = PanelOrientationFailure::Missing;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                failure = failure.combine(io_failure(&error));
                continue;
            }
        };
        let path = entry.path();
        if fs::read_to_string(path.join("status"))
            .ok()
            .is_some_and(|status| status.trim() != "connected")
        {
            continue;
        }
        match fs::read_to_string(path.join("panel_orientation")) {
            Ok(value) => match parse_rotation(&value) {
                Some(orientation) => {
                    return PanelProbe {
                        orientation: Some(orientation),
                        failure,
                    };
                }
                None => failure = failure.combine(PanelOrientationFailure::Invalid),
            },
            Err(error) => failure = failure.combine(io_failure(&error)),
        }
    }
    PanelProbe {
        orientation: None,
        failure,
    }
}

fn probe_dt_rotation(root: &Path) -> PanelProbe {
    fn visit(
        path: &Path,
        depth: usize,
        failure: &mut PanelOrientationFailure,
    ) -> Option<QuarterTurn> {
        if depth > 12 {
            return None;
        }
        // `rotation` is shared by panel and camera bindings. Only a panel node is valid input
        // here; accepting the first property in the tree can silently calibrate from a camera.
        let is_panel = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "panel" || name.starts_with("panel@"));
        if is_panel {
            let rotation = path.join("rotation");
            match fs::read(rotation) {
                Ok(bytes) => {
                    if bytes.len() >= 4 {
                        let value = u32::from_be_bytes(bytes[..4].try_into().ok()?);
                        if let Some(turn) = parse_rotation(&value.to_string()) {
                            return Some(turn);
                        }
                    }
                    *failure = failure.combine(PanelOrientationFailure::Invalid);
                }
                Err(error) => *failure = failure.combine(io_failure(&error)),
            }
        }
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) => {
                *failure = failure.combine(io_failure(&error));
                return None;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    *failure = failure.combine(io_failure(&error));
                    continue;
                }
            };
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => {
                    if let Some(rotation) = visit(&entry.path(), depth + 1, failure) {
                        return Some(rotation);
                    }
                }
                Ok(_) => {}
                Err(error) => *failure = failure.combine(io_failure(&error)),
            }
        }
        None
    }
    let mut failure = PanelOrientationFailure::Missing;
    let orientation = visit(root, 0, &mut failure);
    PanelProbe {
        orientation,
        failure,
    }
}

fn read_sensor_orientation(paths: &LinuxOrientationPaths) -> Option<SensorObservation> {
    let value = match &paths.sensor_orientation_file {
        Some(path) => fs::read_to_string(path).ok()?,
        None => {
            let output = Command::new("/usr/bin/busctl")
                .args([
                    "--system",
                    "--timeout=1s",
                    "get-property",
                    "net.hadess.SensorProxy",
                    "/net/hadess/SensorProxy",
                    "net.hadess.SensorProxy",
                    "AccelerometerOrientation",
                ])
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            String::from_utf8(output.stdout).ok()?
        }
    };
    let value = value.trim().trim_start_matches("s ").trim_matches('"');
    match value {
        "normal" => Some(SensorObservation::Normal),
        "bottom-up" => Some(SensorObservation::UpsideDown),
        "left-up" => Some(SensorObservation::Left),
        "right-up" => Some(SensorObservation::Right),
        "undefined" => Some(SensorObservation::Undefined),
        _ => None,
    }
}

fn descriptor_matrix(sensor: &Sensor) -> Option<MountMatrix> {
    let rows = sensor.mount_matrix.as_ref()?;
    Some([
        rows.first()?.as_slice().try_into().ok()?,
        rows.get(1)?.as_slice().try_into().ok()?,
        rows.get(2)?.as_slice().try_into().ok()?,
    ])
}

fn kernel_matrix(sensor: &Sensor, root: &Path) -> Option<MountMatrix> {
    let path = root
        .join(sensor.iio_device.as_deref()?)
        .join("in_accel_mount_matrix");
    let text = fs::read_to_string(path).ok()?;
    let values: Vec<i8> = text
        .split(|character: char| character == ',' || character == ';' || character.is_whitespace())
        .filter(|part| !part.is_empty())
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    Some([
        values.get(0..3)?.try_into().ok()?,
        values.get(3..6)?.try_into().ok()?,
        values.get(6..9)?.try_into().ok()?,
    ])
}

struct SwitchDevice {
    name: String,
    fd: OwnedFd,
}

impl SwitchDevice {
    fn switch(&self, code: u16) -> io::Result<Option<bool>> {
        let mut supported = [0u8; 16];
        // SAFETY: both ioctls receive a live evdev fd and a writable buffer of the encoded size.
        let rc = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                eviocgbit(EV_SW, supported.len()) as libc::Ioctl,
                supported.as_mut_ptr(),
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        if supported[usize::from(code) / 8] & (1 << (code % 8)) == 0 {
            return Ok(None);
        }
        let mut state = [0u8; 16];
        // SAFETY: same as above; EVIOCGSW returns the current stateful EV_SW bitmap.
        let rc = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                eviocgsw(state.len()) as libc::Ioctl,
                state.as_mut_ptr(),
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(state[usize::from(code) / 8] & (1 << (code % 8)) != 0))
    }
}

fn discover_switch_devices(
    class_root: &Path,
    devices_root: &Path,
) -> io::Result<Vec<SwitchDevice>> {
    fs::read_dir(class_root)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("event"))
        .map(|entry| {
            let name = fs::read_to_string(entry.path().join("device/name"))?;
            let path = devices_root.join(entry.file_name());
            let cpath = CString::new(path.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "evdev path has NUL"))?;
            // SAFETY: cpath is NUL-terminated; a successful fd is immediately owned.
            let raw = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(SwitchDevice {
                name: name.trim().to_owned(),
                // SAFETY: raw is a fresh successful open and has one owner.
                fd: unsafe { OwnedFd::from_raw_fd(raw) },
            })
        })
        .collect()
}

fn parse_switch_code(value: &str) -> Option<u16> {
    match value {
        "SW_LID" => Some(SW_LID),
        "SW_TABLET_MODE" => Some(SW_TABLET_MODE),
        "SW_ROTATE_LOCK" => Some(SW_ROTATE_LOCK),
        _ => value.parse().ok(),
    }
}

const IOC_READ: u64 = 2;
const IOC_NRBITS: u64 = 8;
const IOC_TYPEBITS: u64 = 8;
const IOC_SIZEBITS: u64 = 14;
const IOC_NRSHIFT: u64 = 0;
const IOC_TYPESHIFT: u64 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u64 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u64 = IOC_SIZESHIFT + IOC_SIZEBITS;
const fn ioc(dir: u64, ty: u8, nr: u64, size: u64) -> libc::c_ulong {
    ((dir << IOC_DIRSHIFT)
        | ((ty as u64) << IOC_TYPESHIFT)
        | (nr << IOC_NRSHIFT)
        | (size << IOC_SIZESHIFT)) as libc::c_ulong
}
fn eviocgbit(event_type: u16, len: usize) -> libc::c_ulong {
    ioc(IOC_READ, b'E', 0x20 + u64::from(event_type), len as u64)
}
fn eviocgsw(len: usize) -> libc::c_ulong {
    ioc(IOC_READ, b'E', 0x1b, len as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn dt_fallback_ignores_camera_rotation_and_reads_panel_rotation() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("pf-orientation-dt-{unique}"));
        let camera = root.join("camera@0");
        let panel = root.join("soc/display/panel@0");
        fs::create_dir_all(&camera).unwrap();
        fs::create_dir_all(&panel).unwrap();
        fs::write(camera.join("rotation"), 180_u32.to_be_bytes()).unwrap();
        fs::write(panel.join("rotation"), 270_u32.to_be_bytes()).unwrap();

        assert_eq!(
            probe_dt_rotation(&root).orientation,
            Some(QuarterTurn::Left)
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn descriptor_resolution_selects_one_device_and_refuses_aggregate_or_ambiguity() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("pf-orientation-caps-{unique}"));
        let installed = root.join("devices");
        let only = installed.join("a133/capabilities.toml");
        fs::create_dir_all(only.parent().unwrap()).unwrap();
        fs::write(&only, "[[screens]]\nrole='primary'\nrotation='cw90'\n").unwrap();
        assert_eq!(
            resolve_descriptor_path(None, None, None, &installed).unwrap(),
            only
        );

        let other = installed.join("a523/capabilities.toml");
        fs::create_dir_all(other.parent().unwrap()).unwrap();
        fs::write(&other, "[[screens]]\nrole='primary'\nrotation='cw90'\n").unwrap();
        assert!(resolve_descriptor_path(None, None, None, &installed)
            .unwrap_err()
            .contains("multiple installed"));

        let aggregate = root.join("platform-capabilities.toml");
        fs::write(&aggregate, "schema_version=1\nruntime_abi='1'\n").unwrap();
        let paths = LinuxOrientationPaths {
            descriptor: aggregate,
            ..LinuxOrientationPaths::default()
        };
        assert!(LinuxOrientationSource::open(paths)
            .err()
            .unwrap()
            .contains("screens"));

        fs::remove_dir_all(root).unwrap();
    }
}
