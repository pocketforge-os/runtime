//! The **sensor manager** — accel/gyro via the single physical model + the descriptor
//! `mount_matrix`. Grows the `.2` `SensorHandle` (which carried only the raw injected pose) into
//! the full derived-channel read: pose → device-frame accelerometer (gravity reaction) + gyro,
//! and the chip-frame raw the IIO node would actually hold (`inverse_mount`).
//!
//! ABSENT on the base Pro (a133 omits `[[sensors]]`) ⇒ every read is typed `HardwareAbsent`,
//! never a crash (the graceful-degradation acceptance). On the Pro S (a523 qmi8658) the pose
//! round-trips through [`crate::physical_model`]. Real qmi8658 silicon / calibration / noise is
//! the flash→serial HARDWARE GATE's authority.

use std::sync::Arc;

use crate::backend::{Backend, Pose};
use crate::descriptor::Descriptor;
use crate::error::CapError;
use crate::physical_model::{self, Mat3};

use super::{reconcile_presence, HardwareProbe};

/// One device-agnostic 6-axis inertial sensor object. The descriptor's structural `mount_matrix`
/// is read once at construction; arbitration + the live pose flow through the backend + probe.
pub struct SensorManager {
    backend: Arc<dyn Backend>,
    probe: Arc<dyn HardwareProbe>,
    mount: Mat3,
}

impl SensorManager {
    /// Build the manager from a session's parts (descriptor structure + backend arbitration +
    /// the live-probe seam). The `mount_matrix` is read from the descriptor (identity default).
    pub fn new(
        descriptor: Arc<Descriptor>,
        backend: Arc<dyn Backend>,
        probe: Arc<dyn HardwareProbe>,
    ) -> SensorManager {
        let mount = descriptor.imu_mount_matrix();
        SensorManager { backend, probe, mount }
    }

    /// Is the IMU present (descriptor advertises accel+gyro AND the live probe does not demote it)?
    pub fn present(&self) -> bool {
        reconcile_presence(self.backend.is_present("imu"), &*self.probe, "imu")
    }

    /// Guard: `Ok(())` if the IMU is present, else `HardwareAbsent`.
    fn require(&self) -> Result<(), CapError> {
        if self.present() {
            Ok(())
        } else {
            Err(CapError::HardwareAbsent)
        }
    }

    /// The chip→device mount matrix this device applies (identity unless the descriptor remaps).
    pub fn mount_matrix(&self) -> &Mat3 {
        &self.mount
    }

    /// Read the current pose (orientation in degrees, angular velocity in deg/s) — the arbitrated
    /// state the backend holds (so it is identical over the in-process and broker backends).
    pub fn read_pose(&self) -> Result<Pose, CapError> {
        self.require()?;
        self.backend.get_pose()
    }

    /// The accelerometer reading in the **device frame** (m/s²) — gravity reaction derived from
    /// the pose by the single physical model. Flat ⇒ `(0, 0, +g)`.
    pub fn read_accel(&self) -> Result<[f64; 3], CapError> {
        let p = self.read_pose()?;
        Ok(physical_model::accel_device(p.roll.to_radians(), p.pitch.to_radians()))
    }

    /// The gyroscope reading in the **device frame** (rad/s) — the body angular velocity (the pose
    /// carries deg/s in human units; this converts to the SI rad/s a real gyro reports).
    pub fn read_gyro(&self) -> Result<[f64; 3], CapError> {
        let p = self.read_pose()?;
        Ok(physical_model::gyro_device(
            p.wx.to_radians(),
            p.wy.to_radians(),
            p.wz.to_radians(),
        ))
    }

    /// The accelerometer reading in the **chip frame** (m/s²) — what the raw IIO node holds before
    /// the app re-applies the mount matrix. `device == apply_mount(M, chip)` for the same pose, so
    /// this exercises the full mount-matrix pipeline off-hardware.
    pub fn read_chip_accel(&self) -> Result<[f64; 3], CapError> {
        let device = self.read_accel()?;
        Ok(physical_model::inverse_mount(&self.mount, &device))
    }

    /// Recover the device frame from the chip-frame raw (`apply_mount`), the inverse of
    /// [`read_chip_accel`](Self::read_chip_accel) — the step the app performs.
    pub fn device_from_chip(&self, chip: &[f64; 3]) -> [f64; 3] {
        physical_model::apply_mount(&self.mount, chip)
    }
}
