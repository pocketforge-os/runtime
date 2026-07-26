//! Open one of the pad's RX-only UARTs as a raw 19200 8N1 byte stream.
//!
//! The MCU only ever transmits (no tx/rts/cts is even muxed — `tsp-ozbp.8`), so we open the tty
//! read-only and put it in raw mode: no canonical processing, no echo, no CR/LF translation, 8
//! data bits / no parity / 1 stop, `CLOCAL|CREAD`. `VMIN=0, VTIME=1` makes `read(2)` return after
//! at most 100 ms even with no bytes, so the reader loop can notice a stop request promptly; at
//! ~48 fps real bytes always arrive well inside that window, so it adds no latency to live data.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// Open `path` (e.g. `/dev/ttyS3`) as a raw 19200 8N1 read-only stream.
pub fn open_19200_8n1(path: &str) -> io::Result<File> {
    let cpath = std::ffi::CString::new(path)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "nul in device path"))?;
    // SAFETY: valid C string; read-only, no controlling tty, close-on-exec.
    let raw = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh owned fd from a successful open.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };

    // SAFETY: zeroed termios is a valid starting struct; tcgetattr fills it for a valid fd.
    let mut tio: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd.as_raw_fd(), &mut tio) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: cfmakeraw on a valid, initialized termios.
    unsafe { libc::cfmakeraw(&mut tio) };
    // 8N1, local line, receiver enabled.
    tio.c_cflag &= !(libc::PARENB | libc::CSTOPB | libc::CSIZE | libc::CRTSCTS);
    tio.c_cflag |= libc::CS8 | libc::CLOCAL | libc::CREAD;
    tio.c_iflag &= !(libc::IXON | libc::IXOFF | libc::IXANY);
    // Return from read after ≤100 ms even with no data, so the loop can check the stop flag.
    tio.c_cc[libc::VMIN] = 0;
    tio.c_cc[libc::VTIME] = 1;
    // SAFETY: valid fd + fully-initialized termios.
    if unsafe { libc::cfsetispeed(&mut tio, libc::B19200) } < 0
        || unsafe { libc::cfsetospeed(&mut tio, libc::B19200) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::tcsetattr(fd.as_raw_fd(), libc::TCSANOW, &tio) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // Discard anything already buffered from before we configured the line.
    unsafe { libc::tcflush(fd.as_raw_fd(), libc::TCIFLUSH) };

    Ok(File::from(fd))
}
