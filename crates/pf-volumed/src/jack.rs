//! Headphone-jack route source: the codec's `SW_HEADPHONE_INSERT` input switch.
//!
//! The kernel (kernel-sunxi-7.x#42) exposes the jack as an evdev device whose switch bit is
//! detection-only; the codec's DAPM policy selects the analog route.  The state is therefore read
//! with `EVIOCGSW` (authoritative, immune to missed or grabbed events) and events are only used to
//! wake the service early.

use crate::Output;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Input-device name published by the A133 codec jack (sun4i-codec, kernel-sunxi-7.x#42).
pub const JACK_DEVICE_NAME: &str = "H616 Audio Codec Headphone Jack";
/// `SW_HEADPHONE_INSERT` from `<linux/input-event-codes.h>`.
pub const SW_HEADPHONE_INSERT: usize = 0x02;
const EV_SW: u8 = 0x05;
const SW_BYTES: usize = 8;

#[derive(Debug)]
pub struct RouteError(String);

impl RouteError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for RouteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RouteError {}

/// Where the physical route comes from. Any error means the route is unknown.
pub trait RouteSource {
    fn route(&mut self) -> Result<Output, RouteError>;
}

/// Map the `EVIOCGSW` bitmap to a route. Only the headphone-insert bit matters.
pub fn route_from_switch_bits(bits: &[u8]) -> Result<Output, RouteError> {
    let byte = bits
        .get(SW_HEADPHONE_INSERT / 8)
        .ok_or_else(|| RouteError(String::from("switch bitmap too short")))?;
    Ok(if byte & (1 << (SW_HEADPHONE_INSERT % 8)) != 0 {
        Output::Headphones
    } else {
        Output::Speaker
    })
}

/// Does the `EVIOCGBIT(EV_SW)` capability bitmap include `SW_HEADPHONE_INSERT`?
pub fn advertises_headphone_switch(caps: &[u8]) -> bool {
    caps.get(SW_HEADPHONE_INSERT / 8)
        .is_some_and(|byte| byte & (1 << (SW_HEADPHONE_INSERT % 8)) != 0)
}

/// Find `/dev/input/eventN` whose sysfs `device/name` is exactly [`JACK_DEVICE_NAME`].
/// `Ok(None)` means the device is absent; an unreadable sysfs tree is an error.
pub fn discover(sysfs_input: &Path, dev_input: &Path) -> Result<Option<PathBuf>, RouteError> {
    let entries = fs::read_dir(sysfs_input)
        .map_err(|error| RouteError(format!("read {}: {error}", sysfs_input.display())))?;
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(text) = name.to_str().filter(|text| text.starts_with("event")) else {
            continue;
        };
        if fs::read_to_string(entry.path().join("device/name"))
            .map(|value| value.trim_end() == JACK_DEVICE_NAME)
            .unwrap_or(false)
        {
            found.push(dev_input.join(text));
        }
    }
    found.sort();
    match found.len() {
        0 | 1 => Ok(found.pop()),
        _ => Err(RouteError(format!(
            "{} devices claim the name {JACK_DEVICE_NAME:?}",
            found.len()
        ))),
    }
}

pub struct EvdevJack {
    file: File,
}

impl EvdevJack {
    /// Open the node and prove it advertises `SW_HEADPHONE_INSERT`; an all-zero `EVIOCGSW` from a
    /// device without that switch would otherwise read as "speaker".
    pub fn open(path: &Path) -> Result<Self, RouteError> {
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(|error| RouteError(format!("open {}: {error}", path.display())))?;
        let mut caps = [0_u8; SW_BYTES];
        // SAFETY: the fd is open and `caps` is SW_BYTES long, matching the length in the request.
        let rc = unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                eviocgbit(EV_SW, SW_BYTES) as _,
                caps.as_mut_ptr(),
            )
        };
        if rc < 0 {
            return Err(RouteError(format!(
                "EVIOCGBIT(EV_SW) on {}: {}",
                path.display(),
                io::Error::last_os_error()
            )));
        }
        if !advertises_headphone_switch(&caps) {
            return Err(RouteError(format!(
                "{} does not advertise SW_HEADPHONE_INSERT",
                path.display()
            )));
        }
        Ok(Self { file })
    }

    /// Wait up to `timeout` for a switch event, then drain pending events. The caller re-reads
    /// the state afterwards, so a timeout, a spurious wake and a real event are all handled alike.
    pub fn wait(&mut self, timeout: Duration) -> Result<(), RouteError> {
        let mut poll = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: `poll` points at one initialised pollfd and the count is 1.
        let rc = unsafe { libc::poll(&mut poll, 1, millis) };
        if rc < 0 {
            return Err(RouteError(format!(
                "poll jack: {}",
                io::Error::last_os_error()
            )));
        }
        if poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(RouteError(String::from("jack device went away")));
        }
        let mut sink = [0_u8; 24 * 16];
        loop {
            match (&self.file).read(&mut sink) {
                Ok(0) => return Err(RouteError(String::from("jack device closed"))),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(RouteError(format!("read jack events: {error}"))),
            }
        }
    }
}

/// `EVIOCGSW(len)` = `_IOC(_IOC_READ, 'E', 0x1b, len)` (asm-generic encoding).
const fn eviocgsw(len: usize) -> libc::c_ulong {
    ((2_u64 << 30) | ((len as u64) << 16) | ((b'E' as u64) << 8) | 0x1b) as libc::c_ulong
}

/// `EVIOCGBIT(ev, len)` = `_IOC(_IOC_READ, 'E', 0x20 + ev, len)`.
const fn eviocgbit(ev: u8, len: usize) -> libc::c_ulong {
    ((2_u64 << 30) | ((len as u64) << 16) | ((b'E' as u64) << 8) | (0x20 + ev as u64))
        as libc::c_ulong
}

impl RouteSource for EvdevJack {
    fn route(&mut self) -> Result<Output, RouteError> {
        let mut bits = [0_u8; SW_BYTES];
        // SAFETY: the fd is open and `bits` is SW_BYTES long, matching the length in the request.
        let rc = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                eviocgsw(SW_BYTES) as _,
                bits.as_mut_ptr(),
            )
        };
        if rc < 0 {
            return Err(RouteError(format!(
                "EVIOCGSW: {}",
                io::Error::last_os_error()
            )));
        }
        route_from_switch_bits(&bits)
    }
}
