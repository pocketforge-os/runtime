//! Raw fd primitives for the input hot-path handoff (`tsp-e1b.10`).
//!
//! Two backends need a *file descriptor*, not a value, to cross the facade boundary for input
//! (the SPIKE-1 shared-fd verdict, `wire/WIRE-PROTOCOL.md` §5):
//!
//! * the **in-process backend** [`open_read_fd`]s the platform-provided evdev node directly;
//! * the **broker client** [`recv_fd`]s the `EVIOCGRAB`-grabbed re-emit fd the broker sends over
//!   `SCM_RIGHTS` on the acquire socket (`Acquire("input")`, wire §4.1).
//!
//! This is the facade-side twin of `pf-input-broker::scm` / `::broker::open_read_fd` (which live
//! in the daemon crate that DEPENDS on `pocketforge`, so the facade cannot reach back into it —
//! the layout is re-stated here, and the frozen `SCM_RIGHTS` ancillary-data shape is the shared
//! contract both sides implement). [`send_fd`] exists for the tests that stand up a fake broker.

use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::path::Path;

/// Open a node read-only, non-blocking, close-on-exec — the input consumer's read-fd shape.
/// (`O_NONBLOCK` so a `read()` on an idle device returns `EAGAIN` instead of parking a frame.)
pub fn open_read_fd(path: impl AsRef<Path>) -> io::Result<OwnedFd> {
    let c = std::ffi::CString::new(path.as_ref().as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path has NUL"))?;
    // SAFETY: valid C string; O_CLOEXEC so the fd never leaks across an exec.
    let raw = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh, owned, non-negative fd.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Receive up to `data.len()` payload bytes plus (optionally) one fd from `sock` in one
/// `recvmsg`. Returns the payload byte count and the received fd if one rode along as an
/// `SCM_RIGHTS` control message. Any surplus fds beyond the first are closed (a hostile/buggy
/// peer cannot leak fds into us).
pub fn recv_fd(sock: RawFd, data: &mut [u8]) -> io::Result<(usize, Option<OwnedFd>)> {
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cbuf = vec![0u8; space];

    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = space as _;

    // SAFETY: msg + buffers are valid; we walk cmsgs only within the returned controllen.
    let n = unsafe { libc::recvmsg(sock, &mut msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut got: Option<OwnedFd> = None;
    // SAFETY: cmsg pointers come from CMSG_FIRSTHDR/NXTHDR over our own control buffer.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data_ptr = libc::CMSG_DATA(cmsg) as *const RawFd;
                let payload = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let count = payload / std::mem::size_of::<RawFd>();
                for i in 0..count {
                    let raw = std::ptr::read_unaligned(data_ptr.add(i));
                    if got.is_none() {
                        got = Some(OwnedFd::from_raw_fd(raw));
                    } else {
                        libc::close(raw); // defensive: only one fd is expected
                    }
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    Ok((n as usize, got))
}

/// Send `data` (non-empty) plus one fd over `sock` as a single `sendmsg` with an `SCM_RIGHTS`
/// ancillary message. The mirror of [`recv_fd`]. The real broker daemon has its own send side
/// (`pf-input-broker::scm`); here it exists only to stand up a fake broker in the tests that
/// prove the client fd-receive path device-free — hence `#[cfg(test)]`.
#[cfg(test)]
pub fn send_fd(sock: RawFd, data: &[u8], fd: RawFd) -> io::Result<()> {
    assert!(!data.is_empty(), "SCM_RIGHTS sendmsg needs ≥1 data byte");
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cbuf = vec![0u8; space];

    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = space as _;

    // SAFETY: msg is initialized; cmsg pointers come from CMSG_FIRSTHDR over our own buffer.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::other("no cmsg header"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::copy_nonoverlapping(&fd, libc::CMSG_DATA(cmsg) as *mut RawFd, 1);

        let n = libc::sendmsg(sock, &msg, 0);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
