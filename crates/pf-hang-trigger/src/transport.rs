use crate::{encode_request, TriggerRequest};
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;

fn socket_address(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.is_empty() || bytes.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid hang sampler socket path",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (destination, source) in address.sun_path.iter_mut().zip(bytes) {
        *destination = *source as libc::c_char;
    }
    let length = size_of::<libc::sa_family_t>() + bytes.len() + 1;
    Ok((address, length as libc::socklen_t))
}

fn connect(path: &Path) -> io::Result<UnixStream> {
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let (address, length) = socket_address(path)?;
    let result = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(UnixStream::from(fd))
}

fn send_with_fd(sock: RawFd, data: &[u8], fd: Option<BorrowedFd<'_>>) -> io::Result<()> {
    if data.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SCM_RIGHTS sendmsg needs at least one data byte",
        ));
    }
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    let space = unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0_u8; space];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    if let Some(fd) = fd {
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = space as _;
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            if header.is_null() {
                return Err(io::Error::other("no cmsg header"));
            }
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as _;
            let raw = fd.as_raw_fd();
            std::ptr::copy_nonoverlapping(&raw, libc::CMSG_DATA(header).cast::<RawFd>(), 1);
        }
    }
    let sent = unsafe { libc::sendmsg(sock, &message, libc::MSG_NOSIGNAL) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    if sent as usize != data.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short SCM_RIGHTS sendmsg",
        ));
    }
    Ok(())
}

pub(crate) fn send_request(
    socket: &Path,
    request: &TriggerRequest,
    process: Option<BorrowedFd<'_>>,
) -> io::Result<()> {
    let stream = connect(socket)?;
    let payload = encode_request(request)?;
    send_with_fd(stream.as_raw_fd(), &payload, process)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    fn seqpacket_pair() -> (OwnedFd, OwnedFd) {
        let mut pair = [-1; 2];
        let result = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                pair.as_mut_ptr(),
            )
        };
        assert_eq!(result, 0, "{}", io::Error::last_os_error());
        unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) }
    }

    fn receive(sock: RawFd) -> (Vec<u8>, Option<OwnedFd>) {
        let mut bytes = [0_u8; 32];
        let mut iov = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        let space = unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as u32) } as usize;
        let mut control = vec![0_u8; space];
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = space as _;
        let length = unsafe { libc::recvmsg(sock, &mut message, libc::MSG_CMSG_CLOEXEC) };
        assert!(length >= 0, "{}", io::Error::last_os_error());
        let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        let descriptor = if header.is_null() {
            None
        } else {
            assert_eq!(unsafe { (*header).cmsg_level }, libc::SOL_SOCKET);
            assert_eq!(unsafe { (*header).cmsg_type }, libc::SCM_RIGHTS);
            Some(unsafe { OwnedFd::from_raw_fd(*libc::CMSG_DATA(header).cast::<RawFd>()) })
        };
        (bytes[..length as usize].to_vec(), descriptor)
    }

    #[test]
    fn scm_rights_sender_preserves_payload_and_optional_descriptor() {
        let (sender, receiver) = seqpacket_pair();
        let process = std::fs::File::open("/proc/self").unwrap();
        send_with_fd(sender.as_raw_fd(), b"with-fd", Some(process.as_fd())).unwrap();
        let (payload, descriptor) = receive(receiver.as_raw_fd());
        assert_eq!(payload, b"with-fd");
        assert!(descriptor.is_some());

        send_with_fd(sender.as_raw_fd(), b"without-fd", None).unwrap();
        let (payload, descriptor) = receive(receiver.as_raw_fd());
        assert_eq!(payload, b"without-fd");
        assert!(descriptor.is_none());

        let error = send_with_fd(sender.as_raw_fd(), b"", None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
