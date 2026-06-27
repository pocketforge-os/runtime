//! The device **capability descriptor** (`platform/devices/<id>/capabilities.toml`) — the
//! broker's source of truth for *what to advertise*, and the app's source of truth for
//! *structure* (input rows, led count, skin). It is owned by E1 (`tsp-9sx`) in the `platform`
//! repo and read **read-only** here.
//!
//! Capability **presence is derived from the descriptor with zero per-device code** — exactly
//! the rule the E5 sim's `broker_stub.py` proved: a sensor/actuator row present ⇒ the
//! capability exists; omitted ⇒ hardware-absent. So `a133` (no `[[sensors]]`, no rumble) and
//! `a523` (qmi8658 IMU + pwm-vibrator) differ **only by descriptor data**.
//!
//! Loading is permissive on unknown fields (the schema is owned by E1 and may grow); it does
//! NOT re-validate the schema (that is `pf caps validate` in the platform repo).

use serde::Deserialize;
use std::path::Path;

/// A parsed `capabilities.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct Descriptor {
    pub identity: Identity,
    /// The default "confirm" face button position (a hint; full action mapping is E2's job).
    #[serde(default)]
    pub accept_default: Option<String>,
    #[serde(default)]
    pub inputs: Vec<Input>,
    #[serde(default)]
    pub sensors: Vec<Sensor>,
    #[serde(default)]
    pub actuators: Vec<Actuator>,
}

/// `[identity]` — joins to `profile.toml` by `id`.
#[derive(Debug, Clone, Deserialize)]
pub struct Identity {
    pub id: String,
    pub manufacturer: String,
    pub model: String,
    #[serde(default)]
    pub codename: Option<String>,
    pub sdl_guid: String,
}

/// One `[[inputs]]` row — a drawable, bindable physical control.
#[derive(Debug, Clone, Deserialize)]
pub struct Input {
    pub id: String,
    pub kind: String,
    pub ev_type: String,
    /// One or two canonical Linux input-event codes, comma-joined (e.g. `"ABS_X,ABS_Y"`).
    pub code: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub skin_part: Option<String>,
    #[serde(default)]
    pub ui: Option<String>,
    #[serde(default)]
    pub range: Option<Axis>,
    #[serde(default)]
    pub x: Option<Axis>,
    #[serde(default)]
    pub y: Option<Axis>,
}

/// An analog axis's calibration (mirrors `input_absinfo`).
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Axis {
    pub min: i32,
    pub max: i32,
    #[serde(default)]
    pub flat: i32,
    #[serde(default)]
    pub fuzz: i32,
    #[serde(default)]
    pub resolution: i32,
}

/// One `[[sensors]]` row (accel/gyro/mag/imu/gnss). Presence of a row ⇒ the cap exists.
#[derive(Debug, Clone, Deserialize)]
pub struct Sensor {
    pub id: String,
    pub kind: String,
    pub iio_device: String,
    #[serde(default)]
    pub units: Option<String>,
    /// The chip→device axis-alignment matrix (`device = M · chip`), 3×3 row-major of ±1/0.
    /// Absent ⇒ identity (no remap). The [`crate::managers::sensors::SensorManager`] applies it
    /// so the app reads the DEVICE frame regardless of how the chip is physically mounted.
    #[serde(default)]
    pub mount_matrix: Option<Vec<Vec<i32>>>,
    /// The simulator skin widget hint (e.g. `"tilt_bubble"`), if any.
    #[serde(default)]
    pub ui: Option<String>,
}

/// One `[[actuators]]` row (rumble / led_array).
#[derive(Debug, Clone, Deserialize)]
pub struct Actuator {
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub count: Option<u32>,
    #[serde(default)]
    pub controller: Option<String>,
    #[serde(default)]
    pub sysfs: Option<String>,
}

/// Failure loading or parsing a descriptor.
#[derive(Debug)]
pub enum DescriptorError {
    Io(std::io::Error),
    Parse(toml::de::Error),
}

impl std::fmt::Display for DescriptorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DescriptorError::Io(e) => write!(f, "read: {e}"),
            DescriptorError::Parse(e) => write!(f, "parse: {e}"),
        }
    }
}

impl std::error::Error for DescriptorError {}

impl Descriptor {
    /// Parse a descriptor from a TOML string.
    pub fn from_toml(s: &str) -> Result<Descriptor, DescriptorError> {
        toml::from_str(s).map_err(DescriptorError::Parse)
    }

    /// Load a descriptor from a `capabilities.toml` path.
    pub fn load(path: impl AsRef<Path>) -> Result<Descriptor, DescriptorError> {
        let text = std::fs::read_to_string(path).map_err(DescriptorError::Io)?;
        Descriptor::from_toml(&text)
    }

    // --- descriptor-derived presence predicates (the broker_stub.py port) ---

    fn has_sensor(&self, kinds: &[&str]) -> bool {
        self.sensors.iter().any(|s| {
            let k = s.kind.to_ascii_lowercase();
            kinds.iter().any(|want| k.contains(want))
        })
    }

    fn has_actuator(&self, kinds: &[&str]) -> bool {
        self.actuators.iter().any(|a| {
            let k = a.kind.to_ascii_lowercase();
            kinds.iter().any(|want| k.contains(want))
        })
    }

    /// Is the named capability **present** on this device (descriptor-derived, zero per-device
    /// code)? Hardware caps map to sensor/actuator rows; platform caps (input/entropy/audio/
    /// settings) are constant. Unknown names are not present.
    ///
    /// This is the exact `_CAP_PRESENCE` table from the sim's `broker_stub.py`, plus the
    /// platform-constant caps that have no descriptor row.
    pub fn cap_present(&self, name: &str) -> bool {
        match name.to_ascii_lowercase().as_str() {
            "imu" => self.has_sensor(&["accel", "gyro"]),
            "accelerometer" => self.has_sensor(&["accel"]),
            "gyroscope" => self.has_sensor(&["gyro"]),
            "magnetometer" => self.has_sensor(&["mag"]),
            "location" | "gnss" => self.has_sensor(&["gnss", "gps"]),
            "rumble" | "vibration" => self.has_actuator(&["rumble"]),
            "leds" => self.has_actuator(&["led"]),
            // Platform-constant capabilities (no descriptor hardware row). Input is present iff
            // the device has any input rows (the schema requires ≥1). Entropy/audio/settings
            // are platform services, present on every device.
            "input" => !self.inputs.is_empty(),
            "entropy" | "audio" | "settings" => true,
            _ => false,
        }
    }

    /// The LED count from the descriptor's `led_array` actuator (0 if none).
    pub fn led_count(&self) -> u32 {
        self.actuators
            .iter()
            .find(|a| a.kind.to_ascii_lowercase().contains("led"))
            .and_then(|a| a.count)
            .unwrap_or(0)
    }

    /// The first inertial sensor row (accel/gyro/imu) on this device, if any.
    fn inertial_sensor(&self) -> Option<&Sensor> {
        self.sensors.iter().find(|s| {
            let k = s.kind.to_ascii_lowercase();
            k.contains("accel") || k.contains("gyro") || k.contains("imu")
        })
    }

    /// The IMU's chip→device `mount_matrix` as floats (`device = M · chip`), defaulting to
    /// [`crate::physical_model::IDENTITY_MOUNT`] when the descriptor omits it or the device has
    /// no inertial sensor. A non-3×3 / non-square matrix is treated as identity (the descriptor
    /// validator owns rejecting a malformed matrix; the manager degrades gracefully, never panics).
    pub fn imu_mount_matrix(&self) -> crate::physical_model::Mat3 {
        let rows = match self.inertial_sensor().and_then(|s| s.mount_matrix.as_ref()) {
            Some(m) => m,
            None => return crate::physical_model::IDENTITY_MOUNT,
        };
        if rows.len() != 3 || rows.iter().any(|r| r.len() != 3) {
            return crate::physical_model::IDENTITY_MOUNT;
        }
        let mut m = crate::physical_model::IDENTITY_MOUNT;
        for (r, row) in rows.iter().enumerate() {
            for (c, v) in row.iter().enumerate() {
                m[r][c] = *v as f64;
            }
        }
        m
    }
}
