//! Minimal evdev ioctl helpers (asm-generic `_IOC` encoding — identical on arm64 and x86, so the
//! numbers computed here are correct on the A523 target). Only what the exercisers need.

use libc::{c_int, c_ulong, Ioctl};
use std::os::unix::io::RawFd;

// asm-generic ioctl direction bits.
const IOC_WRITE: c_ulong = 1;
const IOC_READ: c_ulong = 2;

// _IOC(dir, type, nr, size) = (dir<<30)|(size<<16)|(type<<8)|nr  (asm-generic). The result is
// truncated to `libc::Ioctl` (i32 on musl, u64 on gnu) preserving the bit pattern — EVIOCGBIT
// has bit 31 set (IOC_READ<<30) so this is a bit-pattern cast, NOT a value conversion.
const fn ioc(dir: c_ulong, ty: u8, nr: c_ulong, size: c_ulong) -> Ioctl {
    ((dir << 30) | (size << 16) | ((ty as c_ulong) << 8) | nr) as Ioctl
}

const EV: u8 = b'E';

/// `EVIOCGNAME(len)` — read the device name.
pub fn eviocgname(len: usize) -> Ioctl {
    ioc(IOC_READ, EV, 0x06, len as c_ulong)
}

/// `EVIOCGBIT(ev, len)` — read the capability bitmask for event type `ev` (ev=0 ⇒ the EV_* set).
pub fn eviocgbit(ev: u16, len: usize) -> Ioctl {
    ioc(IOC_READ, EV, 0x20 + ev as c_ulong, len as c_ulong)
}

/// `EVIOCSFF` — upload (send) an FF effect (`struct ff_effect`).
pub fn eviocsff() -> Ioctl {
    ioc(IOC_WRITE, EV, 0x80, std::mem::size_of::<libc::ff_effect>() as c_ulong)
}

/// `EVIOCRMFF` — remove an FF effect by id (`int`).
pub fn eviocrmff() -> Ioctl {
    ioc(IOC_WRITE, EV, 0x81, std::mem::size_of::<c_int>() as c_ulong)
}

/// Read the evdev device name for an open fd (best-effort; empty string on failure).
pub fn device_name(fd: RawFd) -> String {
    let mut buf = [0u8; 256];
    let r = unsafe { libc::ioctl(fd, eviocgname(buf.len()), buf.as_mut_ptr()) };
    if r < 0 {
        return String::new();
    }
    let n = r as usize;
    let end = buf[..n.min(buf.len())].iter().position(|&b| b == 0).unwrap_or(n.min(buf.len()));
    String::from_utf8_lossy(&buf[..end]).trim_end().to_string()
}

/// Test bit `n` in a little-endian byte bitmask.
pub fn test_bit(mask: &[u8], n: u16) -> bool {
    let byte = (n / 8) as usize;
    let bit = (n % 8) as u8;
    byte < mask.len() && (mask[byte] & (1 << bit)) != 0
}
