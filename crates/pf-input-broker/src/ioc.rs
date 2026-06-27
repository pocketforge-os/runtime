//! `ioctl` request-code construction — the asm-generic `_IOC` encoding, identical on
//! aarch64 and x86_64 (so the broker built natively and under `qemu-tsp` issues the same
//! request numbers). The fixed `UI_*` codes and the length/struct-sized `EVIOC*`/`UI_GET_SYSNAME`
//! codes are computed here exactly as the proven reference sources do (`sim/spike3/mkuinput.c`,
//! `sim/synth/uinput_synth.py`, `qemu-tsp/regression/probe.c`), so this broker's ioctls are
//! byte-for-byte the ones the qemu-tsp evdev/uinput passthrough patch already validates.

use std::os::raw::c_ulong;

// --- event-type + sync codes (from <linux/input-event-codes.h>; libc omits these) -----------
/// `EV_SYN` — report-boundary event type.
pub const EV_SYN: u16 = 0x00;
/// `EV_KEY` — key/button event type.
pub const EV_KEY: u16 = 0x01;
/// `EV_ABS` — absolute-axis event type.
pub const EV_ABS: u16 = 0x03;
/// `SYN_REPORT` — commit the current event report.
pub const SYN_REPORT: u16 = 0x00;

const IOC_NONE: u64 = 0;
const IOC_WRITE: u64 = 1;
const IOC_READ: u64 = 2;

const IOC_NRBITS: u64 = 8;
const IOC_TYPEBITS: u64 = 8;
const IOC_SIZEBITS: u64 = 14;

const IOC_NRSHIFT: u64 = 0;
const IOC_TYPESHIFT: u64 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u64 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u64 = IOC_SIZESHIFT + IOC_SIZEBITS;

/// `_IOC(dir, type, nr, size)` — the kernel's request-code constructor.
const fn ioc(dir: u64, ty: u8, nr: u64, size: u64) -> c_ulong {
    ((dir << IOC_DIRSHIFT)
        | ((ty as u64) << IOC_TYPESHIFT)
        | (nr << IOC_NRSHIFT)
        | (size << IOC_SIZESHIFT)) as c_ulong
}

// --- evdev ('E' = 0x45) ---------------------------------------------------------------------
const EV: u8 = b'E';

/// `EVIOCGRAB` — exclusive grab (arg: `int` 1 = grab, 0 = release). The heart of v0 enforcement.
pub const EVIOCGRAB: c_ulong = ioc(IOC_WRITE, EV, 0x90, 4);
/// `EVIOCREVOKE` — revoke a previously-handed fd (arg: `int` 0). Used to hard-revoke on policy.
pub const EVIOCREVOKE: c_ulong = ioc(IOC_WRITE, EV, 0x91, 4);

/// `EVIOCGID` — read the `struct input_id` (bus/vendor/product/version).
pub fn eviocgid() -> c_ulong {
    ioc(IOC_READ, EV, 0x02, std::mem::size_of::<libc::input_id>() as u64)
}

/// `EVIOCGNAME(len)` — read the device name into a `len`-byte buffer.
pub fn eviocgname(len: usize) -> c_ulong {
    ioc(IOC_READ, EV, 0x06, len as u64)
}

/// `EVIOCGBIT(ev, len)` — read the capability bitmap for event type `ev` (`ev == 0` ⇒ the
/// set of supported event types).
pub fn eviocgbit(ev: u16, len: usize) -> c_ulong {
    ioc(IOC_READ, EV, 0x20 + ev as u64, len as u64)
}

/// `EVIOCGABS(abs)` — read the `struct input_absinfo` for an absolute axis.
pub fn eviocgabs(abs: u16) -> c_ulong {
    ioc(IOC_READ, EV, 0x40 + abs as u64, std::mem::size_of::<libc::input_absinfo>() as u64)
}

// --- uinput ('U' = 0x55) --------------------------------------------------------------------
const UI: u8 = b'U';

/// `UI_DEV_CREATE` — instantiate the configured virtual device.
pub const UI_DEV_CREATE: c_ulong = ioc(IOC_NONE, UI, 1, 0);
/// `UI_DEV_DESTROY` — tear the virtual device down.
pub const UI_DEV_DESTROY: c_ulong = ioc(IOC_NONE, UI, 2, 0);
/// `UI_SET_EVBIT` — advertise support for an event type (arg: `int`).
pub const UI_SET_EVBIT: c_ulong = ioc(IOC_WRITE, UI, 100, 4);
/// `UI_SET_KEYBIT` — advertise a key/button code (arg: `int`).
pub const UI_SET_KEYBIT: c_ulong = ioc(IOC_WRITE, UI, 101, 4);
/// `UI_SET_ABSBIT` — advertise an absolute axis (arg: `int`).
pub const UI_SET_ABSBIT: c_ulong = ioc(IOC_WRITE, UI, 103, 4);

/// `UI_GET_SYSNAME(len)` — read the created device's `inputN` sysname into a `len`-byte buffer.
pub fn ui_get_sysname(len: usize) -> c_ulong {
    ioc(IOC_READ, UI, 44, len as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Cross-check the computed request numbers against the canonical Linux values (asm-generic).
    #[test]
    fn fixed_codes_match_canonical_linux_values() {
        assert_eq!(EVIOCGRAB, 0x40044590, "EVIOCGRAB = _IOW('E', 0x90, int)");
        assert_eq!(EVIOCREVOKE, 0x40044591, "EVIOCREVOKE = _IOW('E', 0x91, int)");
        assert_eq!(UI_DEV_CREATE, 0x5501, "UI_DEV_CREATE = _IO('U', 1)");
        assert_eq!(UI_DEV_DESTROY, 0x5502, "UI_DEV_DESTROY = _IO('U', 2)");
        assert_eq!(UI_SET_EVBIT, 0x40045564, "UI_SET_EVBIT = _IOW('U', 100, int)");
        assert_eq!(UI_SET_KEYBIT, 0x40045565, "UI_SET_KEYBIT = _IOW('U', 101, int)");
        assert_eq!(UI_SET_ABSBIT, 0x40045567, "UI_SET_ABSBIT = _IOW('U', 103, int)");
    }

    #[test]
    fn length_parameterized_codes_track_size() {
        // EVIOCGNAME(16) = _IOC(READ,'E',0x06,16).
        assert_eq!(eviocgname(16), 0x80104506);
        // EVIOCGBIT(0, 8) = _IOC(READ,'E',0x20,8).
        assert_eq!(eviocgbit(0, 8), 0x80084520);
    }
}
