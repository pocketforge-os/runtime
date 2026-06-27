//! The **single physical model** behind the IMU — a faithful Rust port of the E5 sim's
//! `sensor/physical_model.py` (`tsp-an4.7`). It is the deterministic geometric mapping a
//! pose → the two derived sensor channels the [`crate::managers::sensors::SensorManager`]
//! vends, so the runtime and the simulator agree on what a given orientation *reads*:
//!
//! * **accelerometer** (m/s²) — the specific force a real accel reads **at rest**: the gravity
//!   reaction vector projected into the device frame by the orientation. Flat face-up ⇒
//!   `(0, 0, +g)` (the AVD convention). Linear acceleration (position 2nd derivative) is NOT
//!   modelled in v0 ⇒ 0; noted, same as the sim.
//! * **gyroscope** (rad/s) — the body-frame angular velocity. At rest ⇒ 0 (an honest static
//!   gyro reads ~0, not a fabricated echo of the accel).
//!
//! ## Honesty (R-A)
//!
//! This is a deterministic geometric MODEL of the pose→sensor mapping — it exercises the units
//! pipeline + the descriptor `mount_matrix` transform. It is NOT real qmi8658 silicon, real
//! calibration, noise, bias, or sample timing — those stay the flash→serial **hardware gate's**
//! authority (epic HONESTY CONTRACT). No wall-clock is read (reproducible + byte-identical).
//!
//! ## Conventions (documented so the round-trip is unambiguous; identical to the sim)
//!
//! Device frame: `X` = right, `Y` = up (toward screen top), `Z` = out of the screen toward the
//! viewer. `pitch` = rotation about X (tilt top away/toward you); `roll` = about Y (tilt
//! left/right); `yaw` = about Z (heading) — does NOT change the gravity reading. `g` =
//! 9.80665 m/s². All angles here are **radians**:
//!
//! ```text
//! accel(device) = ( -g·sin(roll),  g·cos(roll)·sin(pitch),  g·cos(roll)·cos(pitch) )
//! ```
//!
//! Arithmetic is plain `*`/`+` with **no FMA contraction** (the sim's C consumer compiles
//! `-ffp-contract=off` to match) so host-expected == app-reported bit-for-bit.

/// Standard gravity, m/s² (the sim's `G`).
pub const G: f64 = 9.80665;

/// A 3×3 axis-alignment matrix (row-major), as carried by a descriptor `mount_matrix`. Mount
/// matrices are ±1/0 axis permutations, so the transpose is the inverse.
pub type Mat3 = [[f64; 3]; 3];

/// The identity mount (no axis remap) — the default when a descriptor omits `mount_matrix`.
pub const IDENTITY_MOUNT: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// 3×3 (row-major) times a 3-vector. Plain mul/add, no FMA contraction (matches the sim's
/// `_matvec`) so the host-predicted value and the app-reported value are bit-identical.
pub fn matvec(m: &Mat3, v: &[f64; 3]) -> [f64; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

/// Transpose of a 3×3 matrix.
pub fn transpose(m: &Mat3) -> Mat3 {
    [
        [m[0][0], m[1][0], m[2][0]],
        [m[0][1], m[1][1], m[2][1]],
        [m[0][2], m[1][2], m[2][2]],
    ]
}

/// `device = M · chip` (M = descriptor `mount_matrix`). Used to PREDICT what the app reports
/// after it applies M to the chip-frame raws it reads from the IIO node (the sim's `apply_mount`).
pub fn apply_mount(m: &Mat3, chip: &[f64; 3]) -> [f64; 3] {
    matvec(m, chip)
}

/// `chip = Mᵀ · device` for an orthonormal axis-alignment M. The IIO synth writes chip-frame
/// raws; the app re-applies M to recover the device frame (the sim's `inverse_mount`).
pub fn inverse_mount(m: &Mat3, device: &[f64; 3]) -> [f64; 3] {
    matvec(&transpose(m), device)
}

/// The gravity-reaction accelerometer reading in the **device frame** (m/s²) for an orientation
/// given in **radians**. Yaw drops out (rotation about the vertical does not change gravity).
pub fn accel_device(roll_rad: f64, pitch_rad: f64) -> [f64; 3] {
    let (cr, sr) = (roll_rad.cos(), roll_rad.sin());
    let (cp, sp) = (pitch_rad.cos(), pitch_rad.sin());
    [-G * sr, G * cr * sp, G * cr * cp]
}

/// The gyroscope reading in the **device frame** (rad/s) — the body angular velocity verbatim.
pub fn gyro_device(wx_rad_s: f64, wy_rad_s: f64, wz_rad_s: f64) -> [f64; 3] {
    [wx_rad_s, wy_rad_s, wz_rad_s]
}

/// The GUI tilt-bubble gesture mapping (the sim's `pose_from_drag`): a normalized drag
/// `(dx, dy)` in `[-1, 1]` → `(pitch, roll)` in **degrees**. `dy` (up) → pitch forward, `dx`
/// (right) → roll right. Returned in degrees so the GUI client and the headless test are
/// literally interchangeable (one model, two clients — the device-free invariant the sim proves).
pub const TILT_BUBBLE_MAX_DEG: f64 = 45.0;

/// Map a tilt-bubble drag to `(pitch_deg, roll_deg)`.
pub fn pose_from_drag(dx: f64, dy: f64) -> (f64, f64) {
    let dx = dx.clamp(-1.0, 1.0);
    let dy = dy.clamp(-1.0, 1.0);
    (dy * TILT_BUBBLE_MAX_DEG, dx * TILT_BUBBLE_MAX_DEG)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: &[f64; 3], b: &[f64; 3]) {
        for i in 0..3 {
            assert!((a[i] - b[i]).abs() < 1e-9, "axis {i}: {} != {}", a[i], b[i]);
        }
    }

    #[test]
    fn flat_face_up_reads_plus_g_on_z() {
        // Rest, level: the AVD convention — gravity reaction is +g out of the screen.
        close(&accel_device(0.0, 0.0), &[0.0, 0.0, G]);
    }

    #[test]
    fn pitch_ninety_puts_gravity_on_y() {
        // Tilt the top fully away: gravity reaction rotates onto +Y.
        close(&accel_device(0.0, std::f64::consts::FRAC_PI_2), &[0.0, G, 0.0]);
    }

    #[test]
    fn roll_ninety_puts_gravity_on_minus_x() {
        // Tilt fully right: gravity reaction rotates onto -X.
        close(&accel_device(std::f64::consts::FRAC_PI_2, 0.0), &[-G, 0.0, 0.0]);
    }

    #[test]
    fn static_gyro_is_zero_but_velocity_passes_through() {
        close(&gyro_device(0.0, 0.0, 0.0), &[0.0, 0.0, 0.0]);
        close(&gyro_device(1.5, -2.0, 0.25), &[1.5, -2.0, 0.25]);
    }

    #[test]
    fn identity_mount_is_a_noop_roundtrip() {
        let v = [1.0, -2.0, 3.5];
        close(&apply_mount(&IDENTITY_MOUNT, &v), &v);
        close(&inverse_mount(&IDENTITY_MOUNT, &v), &v);
    }

    #[test]
    fn mount_roundtrips_for_an_axis_permutation() {
        // A 90° axis swap (X→Y, Y→-X, Z→Z): device = M·chip, chip = Mᵀ·device must round-trip.
        let m: Mat3 = [[0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]];
        let device = accel_device(0.3, -0.2);
        let chip = inverse_mount(&m, &device);
        close(&apply_mount(&m, &chip), &device);
    }

    #[test]
    fn drag_maps_to_pitch_and_roll_degrees() {
        assert_eq!(pose_from_drag(0.0, 0.0), (0.0, 0.0));
        assert_eq!(pose_from_drag(1.0, 1.0), (TILT_BUBBLE_MAX_DEG, TILT_BUBBLE_MAX_DEG));
        // Clamped beyond full deflection.
        assert_eq!(pose_from_drag(2.0, -2.0), (-TILT_BUBBLE_MAX_DEG, TILT_BUBBLE_MAX_DEG));
    }
}
