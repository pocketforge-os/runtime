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
use std::collections::{BTreeMap, BTreeSet};
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
    reported_switch_failures: BTreeSet<String>,
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
            reported_switch_failures: BTreeSet::new(),
            started: Instant::now(),
        })
    }

    fn record_switch_failures(&mut self, failures: impl IntoIterator<Item = SwitchFailure>) {
        for failure in failures {
            let record = format!("source={} reason={}", failure.source, failure.reason);
            if self.reported_switch_failures.insert(record.clone()) {
                eprintln!("pf-session-authorityd: evdev_switch_source_skipped {record}");
            }
        }
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
        let (states, switch_snapshot_complete) = if self.paths.switch_state_file.is_some() {
            (Vec::new(), true)
        } else {
            let mut relevant_codes = BTreeSet::from([SW_ROTATE_LOCK, SW_TABLET_MODE]);
            relevant_codes.extend(
                self.descriptor
                    .joints
                    .iter()
                    .flat_map(|joint| &joint.switches)
                    .filter_map(|switch| parse_switch_code(&switch.code)),
            );
            let discovery = discover_switch_devices(
                &self.paths.input_class,
                &self.paths.input_devices,
                &relevant_codes,
            );
            let mut failures = discovery.failures;
            let states = discovery
                .devices
                .iter()
                .filter_map(|device| match device.snapshot() {
                    Ok(state) => Some(state),
                    Err(error) => {
                        failures.push(SwitchFailure::new(
                            &device.name,
                            format!("read_state_failed:{error}"),
                        ));
                        None
                    }
                })
                .collect();
            let switch_snapshot_complete = failures.is_empty();
            self.record_switch_failures(failures);
            (states, switch_snapshot_complete)
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
        if self.paths.switch_state_file.is_some() {
            inputs.rotate_locks = fixture_values("SW_ROTATE_LOCK")?;
            inputs.tablet_mode = fixture_values("SW_TABLET_MODE")?.into_iter().next();
        } else if switch_snapshot_complete {
            inputs.rotate_locks = states
                .iter()
                .filter_map(|device| device.switch(SW_ROTATE_LOCK))
                .collect();
            inputs.tablet_mode = states
                .iter()
                .find_map(|device| device.switch(SW_TABLET_MODE));
        } else {
            inputs.rotate_locks = vec![true];
            inputs.tablet_mode = Some(false);
        }
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
        let mut source_failures = Vec::new();
        let mut required_source_failed = !switch_snapshot_complete;
        for switch in &joint.switches {
            let Some(code) = parse_switch_code(&switch.code) else {
                required_source_failed = true;
                source_failures.push(SwitchFailure::new(
                    &switch.source,
                    format!("unsupported_switch:{}", switch.code),
                ));
                continue;
            };
            let set = if self.paths.switch_state_file.is_some() {
                fixture_value(&switch.source, &switch.code)?
            } else {
                let Some(device) = states.iter().find(|device| device.name == switch.source) else {
                    required_source_failed = true;
                    source_failures
                        .push(SwitchFailure::new(&switch.source, "required_source_absent"));
                    continue;
                };
                device.switch(code)
            };
            let Some(set) = set else {
                required_source_failed = true;
                source_failures.push(SwitchFailure::new(
                    &switch.source,
                    format!("unsupported_switch:{}", switch.code),
                ));
                continue;
            };
            if code == SW_TABLET_MODE {
                inputs.tablet_mode_is_posture = true;
            }
            if set {
                value = (switch.active[0] + switch.active[1]) / 2.0;
            }
        }
        let proposed = posture_rotation(
            joint,
            if required_source_failed {
                default_value
            } else {
                value
            },
        );
        let now_ms = self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let hall = self
            .hall
            .get_or_insert_with(|| HallDebouncer::new(proposed, 50));
        hall.observe(proposed, now_ms);
        inputs.screen_posture = hall.advance(now_ms);
        self.record_switch_failures(source_failures);
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
    fn snapshot(&self) -> io::Result<SwitchState> {
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
        Ok(SwitchState {
            name: self.name.clone(),
            supported,
            state,
        })
    }
}

struct SwitchState {
    name: String,
    supported: [u8; 16],
    state: [u8; 16],
}

impl SwitchState {
    fn switch(&self, code: u16) -> Option<bool> {
        let byte = usize::from(code) / 8;
        let mask = 1 << (code % 8);
        (self.supported.get(byte)? & mask != 0).then_some(self.state[byte] & mask != 0)
    }
}

struct SwitchFailure {
    source: String,
    reason: String,
}

impl SwitchFailure {
    fn new(source: impl AsRef<str>, reason: impl Into<String>) -> Self {
        Self {
            source: source.as_ref().to_owned(),
            reason: reason.into(),
        }
    }
}

#[derive(Default)]
struct SwitchDiscovery {
    devices: Vec<SwitchDevice>,
    failures: Vec<SwitchFailure>,
}

fn advertises_relevant_switches(path: &Path, relevant_codes: &BTreeSet<u16>) -> io::Result<bool> {
    let contents = fs::read_to_string(path)?;
    let words = contents
        .split_whitespace()
        .map(|word| u64::from_str_radix(word.strip_prefix("0x").unwrap_or(word), 16))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(relevant_codes.iter().any(|code| {
        let word = usize::from(*code) / u64::BITS as usize;
        let bit = usize::from(*code) % u64::BITS as usize;
        words
            .iter()
            .rev()
            .nth(word)
            .is_some_and(|bits| bits & (1u64 << bit) != 0)
    }))
}

fn discover_switch_devices(
    class_root: &Path,
    devices_root: &Path,
    relevant_codes: &BTreeSet<u16>,
) -> SwitchDiscovery {
    let mut discovery = SwitchDiscovery::default();
    let entries = match fs::read_dir(class_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            discovery.failures.push(SwitchFailure::new(
                class_root.to_string_lossy(),
                format!("input_root_unavailable:{error}"),
            ));
            return discovery;
        }
        Err(error) => {
            discovery.failures.push(SwitchFailure::new(
                class_root.to_string_lossy(),
                format!("enumerate_failed:{error}"),
            ));
            return discovery;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                discovery.failures.push(SwitchFailure::new(
                    class_root.to_string_lossy(),
                    format!("entry_failed:{error}"),
                ));
                continue;
            }
        };
        let event = entry.file_name();
        if !event.to_string_lossy().starts_with("event") {
            continue;
        }
        let source = event.to_string_lossy();
        match advertises_relevant_switches(
            &entry.path().join("device/capabilities/sw"),
            relevant_codes,
        ) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(error) => {
                discovery.failures.push(SwitchFailure::new(
                    &source,
                    format!("capabilities_unavailable:{error}"),
                ));
                continue;
            }
        }
        let name = match fs::read_to_string(entry.path().join("device/name")) {
            Ok(name) => name,
            Err(error) => {
                discovery.failures.push(SwitchFailure::new(
                    &source,
                    format!("name_unavailable:{error}"),
                ));
                continue;
            }
        };
        let path = devices_root.join(&event);
        let cpath = match CString::new(path.as_os_str().as_bytes()) {
            Ok(path) => path,
            Err(_) => {
                discovery
                    .failures
                    .push(SwitchFailure::new(&source, "invalid_devnode_path"));
                continue;
            }
        };
        // SAFETY: cpath is NUL-terminated; a successful fd is immediately owned.
        let raw = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if raw < 0 {
            let error = io::Error::last_os_error();
            let reason = match error.raw_os_error() {
                Some(code) if code == libc::ENOENT || code == libc::ENODEV => {
                    format!("devnode_unavailable:{error}")
                }
                _ => format!("open_failed:{error}"),
            };
            discovery.failures.push(SwitchFailure::new(&source, reason));
            continue;
        }
        discovery.devices.push(SwitchDevice {
            name: name.trim().to_owned(),
            // SAFETY: raw is a fresh successful open and has one owner.
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
        });
    }
    discovery
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

    fn switch_discovery_root(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("pf-switch-discovery-{label}-{unique}"));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_switch_capability(root: &Path, event: &str, bits: &str) {
        let device = root.join("class").join(event).join("device");
        fs::create_dir_all(device.join("capabilities")).unwrap();
        fs::write(device.join("name"), format!("{event}\n")).unwrap();
        fs::write(device.join("capabilities/sw"), bits).unwrap();
    }

    fn discover_test_switches(root: &Path) -> SwitchDiscovery {
        discover_switch_devices(
            &root.join("class"),
            &root.join("dev"),
            &BTreeSet::from([SW_LID]),
        )
    }

    #[test]
    fn switch_discovery_skips_a_switch_device_whose_devnode_disappeared() {
        let root = switch_discovery_root("missing-node");
        write_switch_capability(&root, "event0", "1\n");
        fs::create_dir_all(root.join("dev")).unwrap();

        let discovery = discover_test_switches(&root);
        assert!(discovery.devices.is_empty());
        assert_eq!(discovery.failures.len(), 1);
        assert!(discovery.failures[0]
            .reason
            .starts_with("devnode_unavailable:"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn switch_discovery_records_a_missing_input_root() {
        let root = switch_discovery_root("missing-root");

        let discovery = discover_test_switches(&root);
        assert!(discovery.devices.is_empty());
        assert_eq!(discovery.failures.len(), 1);
        assert!(discovery.failures[0]
            .reason
            .starts_with("input_root_unavailable:"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn switch_discovery_does_not_open_a_non_switch_device() {
        let root = switch_discovery_root("non-switch");
        write_switch_capability(&root, "event0", "0\n");
        // A directory is intentionally openable with O_RDONLY. The old unfiltered
        // discovery therefore returned it as a device; capability filtering must not.
        fs::create_dir_all(root.join("dev/event0")).unwrap();

        let discovery = discover_test_switches(&root);
        assert!(discovery.devices.is_empty());
        assert!(discovery.failures.is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn switch_discovery_confines_an_open_error_to_one_source() {
        let root = switch_discovery_root("one-source-error");
        write_switch_capability(&root, "event0", "1\n");
        write_switch_capability(&root, "event1", "1\n");
        fs::create_dir_all(root.join("dev")).unwrap();
        std::os::unix::fs::symlink("event0", root.join("dev/event0")).unwrap();
        fs::write(root.join("dev/event1"), b"").unwrap();

        let discovery = discover_test_switches(&root);
        assert_eq!(discovery.devices.len(), 1);
        assert_eq!(discovery.devices[0].name, "event1");
        assert_eq!(discovery.failures.len(), 1);
        assert!(discovery.failures[0].reason.starts_with("open_failed:"));

        fs::remove_dir_all(root).unwrap();
    }

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
