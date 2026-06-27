//! `SCM_RIGHTS` file-descriptor passing over a Unix socket — the out-of-band layout the wire
//! spec reserves for `Acquire("input")` (`wire/WIRE-PROTOCOL.md` §4.1: "PFW1 itself does not
//! frame the fd; `.6` specifies the ancillary-data layout"). The broker sends the framed PFW1
//! `Response` as the message payload AND the shared re-emit read fd as one `SCM_RIGHTS` control
//! message; the client reads both in one `recvmsg`. The fd, not RPC, is the input hot path.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// Send `data` (the framed response) plus one fd over `sock` as a single `sendmsg` with an
/// `SCM_RIGHTS` ancillary message. `data` must be non-empty (a stream `sendmsg` needs ≥1 byte
/// for the ancillary data to ride along).
pub fn send_fd(sock: RawFd, data: &[u8], fd: RawFd) -> io::Result<()> {
    assert!(!data.is_empty(), "SCM_RIGHTS sendmsg needs ≥1 data byte");
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    // Control buffer sized for exactly one fd.
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

/// Receive up to `data.len()` payload bytes plus (optionally) one fd from `sock`. Returns the
/// number of payload bytes and the received fd if one rode along. Closes any extra fds beyond the
/// first (a hostile/buggy peer cannot leak fds into us).
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
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data_ptr = libc::CMSG_DATA(cmsg) as *const RawFd;
                // Number of fds carried in this cmsg.
                let payload = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let count = payload / std::mem::size_of::<RawFd>();
                for i in 0..count {
                    let raw = std::ptr::read_unaligned(data_ptr.add(i));
                    if got.is_none() {
                        got = Some(OwnedFd::from_raw_fd(raw));
                    } else {
                        // Defensive: only one fd is expected; close any surplus.
                        libc::close(raw);
                    }
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    Ok((n as usize, got))
}

/// `dup` an fd so the owner can keep its copy after handing one out.
pub fn dup_fd(fd: &impl AsRawFd) -> io::Result<OwnedFd> {
    // SAFETY: F_DUPFD_CLOEXEC returns a fresh owned fd or -1.
    let raw = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is a fresh owned fd.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}
