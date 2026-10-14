//! Event I/O boundaries for the broker's one remap and policy loop.
//!
//! Production implements these traits with evdev and uinput. Hermetic callers supply only
//! already-owned process-private descriptors; this module never discovers or opens a device.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};

/// A source of complete native Linux input-event records.
pub trait EventSource {
    /// Read up to `out.len()` complete records. `Ok(0)` means no record is currently available.
    fn read_events(&mut self, out: &mut [libc::input_event]) -> io::Result<usize>;
}

/// The authoritative state query used by the real pump after `SYN_DROPPED`.
///
/// Evdev answers from kernel state. A private byte stream cannot reconstruct state that its
/// producer says was dropped, so it fails that exceptional path explicitly.
pub trait SourceState {
    fn pressed_keys(&self, codes: &[u16]) -> io::Result<Vec<u16>>;
    fn abs_value(&self, code: u16) -> io::Result<i32>;
}

/// A sink for complete native Linux input-event records.
pub trait EventSink {
    fn write_events(&mut self, events: &[libc::input_event]) -> io::Result<()>;
}

/// An event source backed only by a caller-owned pipe or socket descriptor.
pub struct FdEventSource {
    fd: OwnedFd,
    pending: Vec<u8>,
}

impl FdEventSource {
    pub fn from_owned_fd(fd: OwnedFd) -> Self {
        Self {
            fd,
            pending: Vec::new(),
        }
    }

    fn copy_complete_events(&mut self, out: &mut [libc::input_event]) -> usize {
        let event_size = std::mem::size_of::<libc::input_event>();
        let count = (self.pending.len() / event_size).min(out.len());
        let bytes = count * event_size;
        if bytes == 0 {
            return 0;
        }
        // SAFETY: `out[..count]` is valid for exactly `bytes`; copy_nonoverlapping accepts the
        // source's byte alignment and produces byte-for-byte initialized C records.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.pending.as_ptr(),
                out.as_mut_ptr().cast::<u8>(),
                bytes,
            );
        }
        self.pending.drain(..bytes);
        count
    }

    fn wait_readable(&self) -> io::Result<bool> {
        let mut pollfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd; the timeout keeps stop checks responsive.
        let result = unsafe { libc::poll(&mut pollfd, 1, 200) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                return Ok(false);
            }
            return Err(error);
        }
        if result == 0 {
            return Ok(false);
        }
        if pollfd.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "private event source poll failed",
            ));
        }
        // Read through POLLHUP: a final readable packet wins, then read(2) returns zero and the
        // buffered-byte check distinguishes clean EOF from a truncated record.
        Ok(pollfd.revents & (libc::POLLIN | libc::POLLHUP) != 0)
    }
}

impl EventSource for FdEventSource {
    fn read_events(&mut self, out: &mut [libc::input_event]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            let count = self.copy_complete_events(out);
            if count != 0 {
                return Ok(count);
            }
            if !self.wait_readable()? {
                return Ok(0);
            }

            let mut bytes = [0u8; 4096];
            // SAFETY: bytes is writable for its full length and fd remains owned by self.
            let read = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    bytes.as_mut_ptr().cast::<libc::c_void>(),
                    bytes.len(),
                )
            };
            if read > 0 {
                self.pending.extend_from_slice(&bytes[..read as usize]);
                continue;
            }
            if read == 0 {
                return if self.pending.is_empty() {
                    Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "private event source reached EOF",
                    ))
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "private event source reached EOF inside input_event",
                    ))
                };
            }

            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => return Ok(0),
                _ => return Err(error),
            }
        }
    }
}

impl SourceState for FdEventSource {
    fn pressed_keys(&self, _codes: &[u16]) -> io::Result<Vec<u16>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private event source has no kernel state for SYN_DROPPED recovery",
        ))
    }

    fn abs_value(&self, _code: u16) -> io::Result<i32> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private event source has no kernel state for SYN_DROPPED recovery",
        ))
    }
}

/// An event sink backed only by a caller-owned pipe or socket descriptor.
pub struct FdEventSink {
    fd: OwnedFd,
}

impl FdEventSink {
    pub fn from_owned_fd(fd: OwnedFd) -> Self {
        Self { fd }
    }
}

impl EventSink for FdEventSink {
    fn write_events(&mut self, events: &[libc::input_event]) -> io::Result<()> {
        // SAFETY: input_event contains no references; this view is bounded by `events`.
        let bytes = unsafe {
            std::slice::from_raw_parts(events.as_ptr().cast::<u8>(), std::mem::size_of_val(events))
        };
        let mut written = 0;
        while written < bytes.len() {
            // SAFETY: the remaining slice is readable and fd remains owned by self.
            let count = unsafe {
                libc::write(
                    self.fd.as_raw_fd(),
                    bytes[written..].as_ptr().cast::<libc::c_void>(),
                    bytes.len() - written,
                )
            };
            if count > 0 {
                written += count as usize;
                continue;
            }
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "private event sink wrote zero bytes",
                ));
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINTR) {
                return Err(error);
            }
        }
        Ok(())
    }
}

/// In-memory event sink for device-free tests.
#[derive(Default)]
pub struct RecordingSink {
    events: Vec<libc::input_event>,
}

impl RecordingSink {
    pub fn events(&self) -> &[libc::input_event] {
        &self.events
    }

    pub fn into_events(self) -> Vec<libc::input_event> {
        self.events
    }
}

impl EventSink for RecordingSink {
    fn write_events(&mut self, events: &[libc::input_event]) -> io::Result<()> {
        self.events.extend_from_slice(events);
        Ok(())
    }
}
