//! `ioctl` request-code construction for the uinput setup path — the asm-generic `_IOC` encoding,
//! carried self-contained (typed as [`libc::Ioctl`], the `pf-input-collect` / `pf-hw-exerciser`
//! precedent) so this device binary cross-compiles to `aarch64-unknown-linux-musl` (where
//! `Ioctl == i32`) as well as the native gnu build (`Ioctl == u64`), and so it takes NO dependency
//! on `pf-input-broker` (which sits above us). The computed numbers are cross-checked against the
//! canonical Linux values in the tests.

use libc::{c_ulong, Ioctl};

const IOC_NONE: c_ulong = 0;
const IOC_WRITE: c_ulong = 1;
const IOC_READ: c_ulong = 2;

/// `_IOC(dir, type, nr, size)` truncated into `libc::Ioctl` preserving the bit pattern — the same
/// construction `pf-input-collect::source::ioc` and `evdev-probe.py` use.
const fn ioc(dir: c_ulong, ty: u8, nr: c_ulong, size: c_ulong) -> Ioctl {
    ((dir << 30) | (size << 16) | ((ty as c_ulong) << 8) | nr) as Ioctl
}

const UI: u8 = b'U';

/// `UI_DEV_CREATE` — instantiate the configured virtual device.
pub const UI_DEV_CREATE: Ioctl = ioc(IOC_NONE, UI, 1, 0);
/// `UI_DEV_DESTROY` — tear the virtual device down.
pub const UI_DEV_DESTROY: Ioctl = ioc(IOC_NONE, UI, 2, 0);
/// `UI_SET_EVBIT` — advertise support for an event type (arg: `int`).
pub const UI_SET_EVBIT: Ioctl = ioc(IOC_WRITE, UI, 100, 4);
/// `UI_SET_KEYBIT` — advertise a key/button code (arg: `int`).
pub const UI_SET_KEYBIT: Ioctl = ioc(IOC_WRITE, UI, 101, 4);
/// `UI_SET_ABSBIT` — advertise an absolute axis (arg: `int`).
pub const UI_SET_ABSBIT: Ioctl = ioc(IOC_WRITE, UI, 103, 4);

/// `UI_GET_SYSNAME(len)` — read the created device's `inputN` sysname into a `len`-byte buffer.
pub fn ui_get_sysname(len: usize) -> Ioctl {
    ioc(IOC_READ, UI, 44, len as c_ulong)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Cross-check the computed request numbers against the canonical Linux values (asm-generic).
    // Compared as u32 bit patterns so the assertion is identical on musl (i32) and gnu (u64).
    #[test]
    fn fixed_codes_match_canonical_linux_values() {
        assert_eq!(UI_DEV_CREATE as u32, 0x5501, "UI_DEV_CREATE = _IO('U', 1)");
        assert_eq!(UI_DEV_DESTROY as u32, 0x5502, "UI_DEV_DESTROY = _IO('U', 2)");
        assert_eq!(UI_SET_EVBIT as u32, 0x40045564, "UI_SET_EVBIT = _IOW('U', 100, int)");
        assert_eq!(UI_SET_KEYBIT as u32, 0x40045565, "UI_SET_KEYBIT = _IOW('U', 101, int)");
        assert_eq!(UI_SET_ABSBIT as u32, 0x40045567, "UI_SET_ABSBIT = _IOW('U', 103, int)");
    }

    #[test]
    fn ui_get_sysname_tracks_len() {
        // UI_GET_SYSNAME(64) = _IOC(READ, 'U', 44, 64).
        assert_eq!(ui_get_sysname(64) as u32, 0x8040552c);
    }
}
