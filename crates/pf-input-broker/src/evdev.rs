//! The **evdev source** — the real `/dev/input/eventN` node the broker takes EXCLUSIVE control
//! of via `EVIOCGRAB`. Once grabbed, the kernel delivers that device's events ONLY to this fd:
//! any other process that opens the node reads nothing. THAT is the v0 enforcement — the app
//! cannot bypass the broker to the raw device (R-B), on the vendor 4.9 kernel, with no namespaces.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use crate::ioc;

/// An opened evdev source device.
pub struct Evdev {
    fd: OwnedFd,
    grabbed: bool,
}

impl Evdev {
    /// Open an evdev node for reading (non-blocking + close-on-exec).
    pub fn open(path: impl AsRef<Path>) -> io::Result<Evdev> {
        let cpath = std::ffi::CString::new(path.as_ref().as_os_str().as_encoded_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path has NUL"))?;
        // SAFETY: cpath is a valid NUL-terminated C string for the lifetime of the call.
        let raw = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh, owned fd from a successful open.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        Ok(Evdev { fd, grabbed: false })
    }

    /// Take the exclusive `EVIOCGRAB`. After this, no other opener of the node receives events.
    pub fn grab(&mut self) -> io::Result<()> {
        // SAFETY: EVIOCGRAB takes an int by value (1 = grab); fd is valid.
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), ioc::EVIOCGRAB, 1 as libc::c_int) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        self.grabbed = true;
        Ok(())
    }

    /// Release the grab (idempotent).
    pub fn ungrab(&mut self) -> io::Result<()> {
        if !self.grabbed {
            return Ok(());
        }
        // SAFETY: EVIOCGRAB with 0 releases; fd is valid.
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), ioc::EVIOCGRAB, 0 as libc::c_int) };
        self.grabbed = false;
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// The device's `EVIOCGNAME` string.
    pub fn name(&self) -> io::Result<String> {
        let mut buf = [0u8; 256];
        // SAFETY: buf is a valid writable buffer of the length passed to the ioctl.
        let n = unsafe {
            libc::ioctl(self.fd.as_raw_fd(), ioc::eviocgname(buf.len()), buf.as_mut_ptr())
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(n as usize);
        Ok(String::from_utf8_lossy(&buf[..end]).into_owned())
    }

    /// The device's `EVIOCGID` identity `(bus, vendor, product, version)`.
    pub fn id(&self) -> io::Result<(u16, u16, u16, u16)> {
        let mut id: libc::input_id = unsafe { std::mem::zeroed() };
        // SAFETY: &mut id is a valid input_id-sized buffer matching the ioctl's size field.
        let rc = unsafe {
            libc::ioctl(self.fd.as_raw_fd(), ioc::eviocgid(), &mut id as *mut libc::input_id)
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((id.bustype, id.vendor, id.product, id.version))
    }

    /// Read up to `out.len()` pending events into `out`; returns the count read (0 on `EAGAIN`,
    /// i.e. nothing pending right now — the device is non-blocking).
    pub fn read_events(&self, out: &mut [libc::input_event]) -> io::Result<usize> {
        let cap = std::mem::size_of_val(out);
        // SAFETY: out is a valid buffer of `cap` bytes; read writes at most `cap`.
        let n = unsafe { libc::read(self.fd.as_raw_fd(), out.as_mut_ptr() as *mut libc::c_void, cap) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EAGAIN) {
                return Ok(0);
            }
            return Err(e);
        }
        Ok(n as usize / std::mem::size_of::<libc::input_event>())
    }
}

impl AsRawFd for Evdev {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl Drop for Evdev {
    fn drop(&mut self) {
        let _ = self.ungrab();
    }
}
