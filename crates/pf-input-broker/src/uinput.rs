//! The **uinput sink** — the VIRTUAL device the broker re-emits the remapped, rate-limited
//! event stream into. The app reads this node (or the fd handed to it via `SCM_RIGHTS`); it is
//! indistinguishable from a real evdev device, but every event passed through the broker's policy
//! first. Built from a [`UinputSpec`] derived from the device descriptor (zero per-device code).
//!
//! Uses the legacy `write(uinput_user_dev)` + `UI_DEV_CREATE` path — the exact shape the E5 sim's
//! `uinput_synth.py` + `mkuinput.c` use and that the `qemu-tsp` passthrough already validates.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::ioc;

/// Absolute-axis calibration (mirrors `input_absinfo`).
#[derive(Debug, Clone, Copy)]
pub struct AbsInfo {
    pub min: i32,
    pub max: i32,
    pub fuzz: i32,
    pub flat: i32,
}

/// A complete virtual-device description (codes + axis calibration + identity).
#[derive(Debug, Clone)]
pub struct UinputSpec {
    pub name: String,
    pub bus: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
    /// Key/button codes to advertise (already remapped to the canonical layout).
    pub keys: Vec<u16>,
    /// Absolute axes to advertise, with calibration.
    pub abs: Vec<(u16, AbsInfo)>,
}

/// A live uinput virtual device. `Drop` destroys it.
pub struct Uinput {
    fd: OwnedFd,
    node: Option<String>,
}

impl Uinput {
    /// Configure + instantiate the virtual device from `spec`.
    pub fn create(spec: &UinputSpec) -> io::Result<Uinput> {
        let path = std::ffi::CString::new("/dev/uinput").unwrap();
        // SAFETY: valid C string; standard uinput open.
        let raw = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fresh owned fd from a successful open.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let dev = Uinput { fd, node: None };
        let f = dev.fd.as_raw_fd();

        dev.set_bit(ioc::UI_SET_EVBIT, ioc::EV_SYN as libc::c_int)?;
        if !spec.keys.is_empty() {
            dev.set_bit(ioc::UI_SET_EVBIT, ioc::EV_KEY as libc::c_int)?;
            for &k in &spec.keys {
                dev.set_bit(ioc::UI_SET_KEYBIT, k as libc::c_int)?;
            }
        }
        if !spec.abs.is_empty() {
            dev.set_bit(ioc::UI_SET_EVBIT, ioc::EV_ABS as libc::c_int)?;
            for &(code, _) in &spec.abs {
                dev.set_bit(ioc::UI_SET_ABSBIT, code as libc::c_int)?;
            }
        }

        // Legacy setup: write a uinput_user_dev, then UI_DEV_CREATE.
        let mut uud: libc::uinput_user_dev = unsafe { std::mem::zeroed() };
        let name = spec.name.as_bytes();
        let n = name.len().min(libc::UINPUT_MAX_NAME_SIZE - 1);
        for (dst, &b) in uud.name.iter_mut().zip(&name[..n]) {
            *dst = b as libc::c_char;
        }
        uud.id.bustype = spec.bus;
        uud.id.vendor = spec.vendor;
        uud.id.product = spec.product;
        uud.id.version = spec.version;
        for &(code, ai) in &spec.abs {
            let c = code as usize;
            if c < uud.absmax.len() {
                uud.absmax[c] = ai.max;
                uud.absmin[c] = ai.min;
                uud.absfuzz[c] = ai.fuzz;
                uud.absflat[c] = ai.flat;
            }
        }
        let bytes = std::mem::size_of::<libc::uinput_user_dev>();
        // SAFETY: writing exactly sizeof(uinput_user_dev) from a valid, fully-initialized struct.
        let w = unsafe { libc::write(f, &uud as *const _ as *const libc::c_void, bytes) };
        if w != bytes as isize {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: UI_DEV_CREATE takes no argument.
        if unsafe { libc::ioctl(f, ioc::UI_DEV_CREATE) } < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut dev = dev;
        dev.node = dev.resolve_node();
        Ok(dev)
    }

    fn set_bit(&self, req: libc::c_ulong, bit: libc::c_int) -> io::Result<()> {
        // SAFETY: UI_SET_* take an int by value; fd is valid.
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), req, bit) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Resolve the created device's `/dev/input/eventN` path via `UI_GET_SYSNAME` (`inputN`),
    /// matching the sysfs `device` symlink — the same scheme as `uinput_synth.py::_resolve_node`.
    fn resolve_node(&self) -> Option<String> {
        let mut buf = [0u8; 64];
        // SAFETY: buf is a valid writable buffer of the length passed in the ioctl.
        let n = unsafe {
            libc::ioctl(self.fd.as_raw_fd(), ioc::ui_get_sysname(buf.len()), buf.as_mut_ptr())
        };
        let sysname = if n >= 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            String::from_utf8_lossy(&buf[..end]).trim().to_string()
        } else {
            String::new()
        };
        if sysname.is_empty() {
            return None;
        }
        // The eventN child appears just after UI_DEV_CREATE; poll briefly.
        for _ in 0..50 {
            if let Ok(rd) = std::fs::read_dir("/sys/class/input") {
                for ent in rd.flatten() {
                    let name = ent.file_name();
                    let name = name.to_string_lossy();
                    if !name.starts_with("event") {
                        continue;
                    }
                    let devlink = ent.path().join("device");
                    if let Ok(target) = std::fs::read_link(&devlink) {
                        if target.file_name().map(|f| f.to_string_lossy() == sysname).unwrap_or(false) {
                            return Some(format!("/dev/input/{name}"));
                        }
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        None
    }

    /// The `/dev/input/eventN` node of the re-emit device (if it could be resolved).
    pub fn node(&self) -> Option<&str> {
        self.node.as_deref()
    }

    /// Emit one event (no SYN — call [`syn`](Self::syn) to commit a report).
    pub fn emit(&self, ev_type: u16, code: u16, value: i32) -> io::Result<()> {
        let mut ie: libc::input_event = unsafe { std::mem::zeroed() };
        ie.type_ = ev_type;
        ie.code = code;
        ie.value = value;
        let bytes = std::mem::size_of::<libc::input_event>();
        // SAFETY: writing exactly sizeof(input_event) from a valid struct to the uinput fd.
        let w = unsafe { libc::write(self.fd.as_raw_fd(), &ie as *const _ as *const libc::c_void, bytes) };
        if w != bytes as isize {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Commit the current report (`EV_SYN`/`SYN_REPORT`).
    pub fn syn(&self) -> io::Result<()> {
        self.emit(ioc::EV_SYN, ioc::SYN_REPORT, 0)
    }
}

impl AsRawFd for Uinput {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl Drop for Uinput {
    fn drop(&mut self) {
        // SAFETY: UI_DEV_DESTROY takes no argument; fd valid until the OwnedFd drops next.
        unsafe {
            libc::ioctl(self.fd.as_raw_fd(), ioc::UI_DEV_DESTROY);
        }
    }
}
