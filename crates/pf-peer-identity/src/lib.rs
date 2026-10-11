//! Kernel-attested peer identity for PocketForge control-plane sockets.
//!
//! This is the ONE implementation of "who is on the other end of this Unix socket" (owner
//! condition for tsp-mc9m.41.996.5): `pf-prefsd` classifies preference writers with it, the
//! session authority authenticates the SDK front and re-derives an app's identity from the
//! process handle the front forwards, and anything else that identifies apps must use it too.
//!
//! Identity never comes from a request payload. A connection is identified by socket-bound
//! `SO_PEERCRED` (uid/pid) and `SO_PEERGROUPS`, and a process is pinned by the strongest stable
//! handle the running kernel offers: the socket's own `SO_PEERPIDFD` (Linux 6.5+), `pidfd_open`
//! (the shipping A523/5.15 kernel) or an opened `/proc/<pid>` directory (the shipping A133/4.9
//! kernel, which has no pidfds). Every cgroup read is bracketed by liveness checks on that handle,
//! so a PID reused after exit can never substitute another process's `/proc` entry. Only the
//! specific "unsupported" errors select a compatibility handle; every other error fails closed.
//!
//! A handle received over `SCM_RIGHTS` ([`ReceivedProcess`]) is classified from the kernel's own
//! description of the descriptor (`anon_inode:[pidfd]` or `/proc/<pid>`), never from the sender.

use std::ffi::{CStr, CString};
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

/// The peer's kernel-attested Unix credentials.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerCred {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

/// Read `SO_PEERCRED` from an accepted Unix connection.
pub fn peer_cred(stream: &UnixStream) -> io::Result<PeerCred> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the fd is a live Unix socket and `cred` is writable for exactly `len` bytes.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(PeerCred {
        pid: cred.pid,
        uid: cred.uid,
        gid: cred.gid,
    })
}

/// Check a credential against an expected uid. Kept separate for direct unit testing.
pub fn verify_peer_uid(cred: PeerCred, allowed_uid: u32) -> io::Result<()> {
    if cred.uid == allowed_uid {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refused peer pid={} uid={} (expected uid={allowed_uid})",
                cred.pid, cred.uid
            ),
        ))
    }
}

/// Which stable handle pins the peer process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerProcessKind {
    SocketPidFd,
    PidFdOpen,
    ProcDir,
}

/// A stable handle on the peer process, acquired by [`acquire_peer_process`].
pub struct PeerProcess {
    pub fd: OwnedFd,
    pub kind: PeerProcessKind,
}

/// Kernel facilities behind peer identification, injectable so hermetic tests can model the
/// shipping kernels and unexpected failures without those kernels.
pub trait PeerProcessSource {
    fn peer_groups(&self, stream: &UnixStream) -> io::Result<Vec<u32>>;
    fn socket_pidfd(&self, stream: &UnixStream) -> io::Result<OwnedFd>;
    fn pidfd_open(&self, pid: i32) -> io::Result<OwnedFd>;
    fn proc_dir_open(&self, pid: i32) -> io::Result<OwnedFd>;
    fn cgroup(&self, process: &PeerProcess, pid: i32) -> io::Result<String>;
}

/// The production [`PeerProcessSource`]: the running kernel.
pub struct KernelPeerProcessSource;

impl PeerProcessSource for KernelPeerProcessSource {
    fn peer_groups(&self, stream: &UnixStream) -> io::Result<Vec<u32>> {
        socket_peer_groups(stream)
    }

    fn socket_pidfd(&self, stream: &UnixStream) -> io::Result<OwnedFd> {
        socket_peer_pidfd(stream)
    }

    fn pidfd_open(&self, pid: i32) -> io::Result<OwnedFd> {
        pidfd_open(pid)
    }

    fn proc_dir_open(&self, pid: i32) -> io::Result<OwnedFd> {
        proc_dir_open(pid)
    }

    fn cgroup(&self, process: &PeerProcess, pid: i32) -> io::Result<String> {
        match process.kind {
            PeerProcessKind::SocketPidFd | PeerProcessKind::PidFdOpen => {
                pidfd_cgroup(&process.fd, pid)
            }
            PeerProcessKind::ProcDir => proc_dir_cgroup(&process.fd, pid),
        }
    }
}

/// Open a pidfd for `pid` (Linux 5.3+).
pub fn pidfd_open(pid: i32) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open takes a numeric PID and zero flags, and returns a new owned fd.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) })
}

/// Open `/proc/<pid>` as a directory handle (the 4.9 compatibility pin).
pub fn proc_dir_open(pid: i32) -> io::Result<OwnedFd> {
    let path = CString::new(format!("/proc/{pid}"))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    // SAFETY: `path` is NUL-terminated and the successful descriptor is owned by the caller.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn pidfd_cgroup(pidfd: &OwnedFd, pid: i32) -> io::Result<String> {
    verify_live_pidfd(pidfd, pid)?;
    let cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
    verify_live_pidfd(pidfd, pid)?;
    Ok(cgroup)
}

fn proc_dir_cgroup(proc_dir: &OwnedFd, pid: i32) -> io::Result<String> {
    verify_proc_dir(proc_dir, pid)?;
    let cgroup = read_proc_file_at(proc_dir, c"cgroup")?;
    verify_proc_dir(proc_dir, pid)?;
    Ok(cgroup)
}

/// Read socket-bound `SO_PEERGROUPS` from an accepted Unix connection.
pub fn socket_peer_groups(stream: &UnixStream) -> io::Result<Vec<u32>> {
    // SAFETY: sysconf has no pointer arguments and only queries a process limit.
    let max_groups = unsafe { libc::sysconf(libc::_SC_NGROUPS_MAX) };
    if !(0..=65_536).contains(&max_groups) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid NGROUPS_MAX: {max_groups}"),
        ));
    }
    let mut groups = vec![0 as libc::gid_t; max_groups as usize];
    let mut len = groups
        .len()
        .checked_mul(std::mem::size_of::<libc::gid_t>())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "peer groups overflow"))?
        as libc::socklen_t;
    // SAFETY: the fd is a live Unix socket and `groups` is writable for exactly `len` bytes.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERGROUPS,
            groups.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    let gid_size = std::mem::size_of::<libc::gid_t>();
    if len as usize % gid_size != 0 || len as usize > groups.len() * gid_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SO_PEERGROUPS returned an invalid length",
        ));
    }
    groups.truncate(len as usize / gid_size);
    Ok(groups)
}

fn socket_peer_pidfd(stream: &UnixStream) -> io::Result<OwnedFd> {
    let mut fd: libc::c_int = -1;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: the fd is a live Unix socket and `fd` is writable for exactly `len` bytes.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERPIDFD,
            (&mut fd as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    if fd < 0 || len as usize != std::mem::size_of::<libc::c_int>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SO_PEERPIDFD returned an invalid descriptor",
        ));
    }
    // SAFETY: successful SO_PEERPIDFD returns a new descriptor owned by the caller.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn read_proc_file_at(proc_dir: &OwnedFd, name: &CStr) -> io::Result<String> {
    // SAFETY: `proc_dir` is an open directory and `name` is a NUL-terminated relative name.
    let fd = unsafe {
        libc::openat(
            proc_dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut contents = String::new();
    File::from(unsafe { OwnedFd::from_raw_fd(fd) }).read_to_string(&mut contents)?;
    Ok(contents)
}

fn proc_dir_pid(proc_dir: &OwnedFd) -> io::Result<i32> {
    let stat = read_proc_file_at(proc_dir, c"stat")?;
    stat.split_once(' ')
        .map(|(pid, _)| pid)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "process stat has no pid"))?
        .parse::<i32>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn verify_proc_dir(proc_dir: &OwnedFd, expected_pid: i32) -> io::Result<()> {
    let pid = proc_dir_pid(proc_dir)?;
    if pid == expected_pid {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("proc directory names pid={pid}, expected peer pid={expected_pid}"),
        ))
    }
}

fn socket_pidfd_unsupported(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOPROTOOPT | libc::EINVAL | libc::EOPNOTSUPP)
    )
}

/// Pin the peer process behind `stream` (whose `SO_PEERCRED` pid is `pid`) with the strongest
/// handle `source` supports. Only the narrow unsupported errors select a weaker handle.
pub fn acquire_peer_process<S: PeerProcessSource>(
    stream: &UnixStream,
    pid: i32,
    source: &S,
) -> io::Result<PeerProcess> {
    match source.socket_pidfd(stream) {
        Ok(fd) => Ok(PeerProcess {
            fd,
            kind: PeerProcessKind::SocketPidFd,
        }),
        Err(error) if socket_pidfd_unsupported(&error) => match source.pidfd_open(pid) {
            Ok(fd) => Ok(PeerProcess {
                fd,
                kind: PeerProcessKind::PidFdOpen,
            }),
            Err(error) if error.raw_os_error() == Some(libc::ENOSYS) => {
                source.proc_dir_open(pid).map(|fd| PeerProcess {
                    fd,
                    kind: PeerProcessKind::ProcDir,
                })
            }
            Err(error) => Err(error),
        },
        Err(error) => Err(error),
    }
}

fn pidfd_pid(pidfd: &OwnedFd) -> io::Result<i32> {
    let fdinfo = fs::read_to_string(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd()))?;
    fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("Pid:\t"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "pidfd has no Pid field"))?
        .trim()
        .parse::<i32>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn verify_live_pidfd(pidfd: &OwnedFd, expected_pid: i32) -> io::Result<()> {
    let pid = pidfd_pid(pidfd)?;
    if pid != expected_pid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("pidfd names pid={pid}, expected peer pid={expected_pid}"),
        ));
    }
    let mut pollfd = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `pollfd` points to one initialized descriptor and the zero timeout never blocks.
    let rc = unsafe { libc::poll(&mut pollfd, 1, 0) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    if rc != 0 || pollfd.revents != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "peer exited during identity lookup",
        ));
    }
    Ok(())
}

/// Read the cgroup document of the process behind `stream`, pinned by a stable handle for the
/// duration of the read. `pid` is the socket's `SO_PEERCRED` pid.
pub fn peer_cgroup_with_source<S: PeerProcessSource>(
    stream: &UnixStream,
    pid: i32,
    source: &S,
) -> io::Result<String> {
    let process = acquire_peer_process(stream, pid, source)?;
    source.cgroup(&process, pid)
}

/// The systemd service unit (`<name>.service`) a `/proc/<pid>/cgroup` document places the
/// process in, taken only from the systemd hierarchy (v2 `0::` or v1 `name=systemd`).
pub fn service_unit_from_cgroup(cgroup: &str) -> Option<String> {
    cgroup.lines().find_map(|line| {
        let mut fields = line.splitn(3, ':');
        let hierarchy = fields.next()?;
        let controllers = fields.next()?;
        let path = fields.next()?;
        let is_systemd = (hierarchy == "0" && controllers.is_empty())
            || controllers.split(',').any(|name| name == "name=systemd");
        if !is_systemd {
            return None;
        }
        path.rsplit('/')
            .find(|component| component.ends_with(".service"))
            .map(str::to_owned)
    })
}

/// The application id of a `pf-app@<id>.service` unit. Anything else, including an empty or
/// non-identifier instance, is not an app.
pub fn app_id_from_unit(unit: &str) -> Option<String> {
    let instance = unit
        .strip_prefix("pf-app@")
        .and_then(|rest| rest.strip_suffix(".service"))?;
    let identifier = !instance.is_empty()
        && instance
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'));
    identifier.then(|| instance.to_owned())
}

/// Whether a service unit is the same unit, by exact name.
pub fn unit_matches(cgroup: &str, expected_unit: &str) -> bool {
    service_unit_from_cgroup(cgroup).as_deref() == Some(expected_unit)
}

/// Authenticate an accepted connection as one specific trusted service: socket-bound uid plus
/// the peer's systemd unit, read through a stable process handle.
pub fn verify_peer_service(
    stream: &UnixStream,
    expected_uid: u32,
    expected_unit: &str,
) -> io::Result<()> {
    let cred = peer_cred(stream)?;
    verify_peer_uid(cred, expected_uid)?;
    let cgroup = peer_cgroup_with_source(stream, cred.pid, &KernelPeerProcessSource)?;
    if unit_matches(&cgroup, expected_unit) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "peer pid={} is {} (expected {expected_unit})",
                cred.pid,
                service_unit_from_cgroup(&cgroup).unwrap_or_else(|| "no service unit".into())
            ),
        ))
    }
}

/// A process handle that arrived over `SCM_RIGHTS`, classified from the kernel's description of
/// the descriptor. The sender chooses nothing: a pidfd is `anon_inode:[pidfd]` and a 4.9-era
/// pin is an opened `/proc/<pid>` directory; everything else is rejected.
#[derive(Debug)]
pub struct ReceivedProcess {
    fd: OwnedFd,
    kind: PeerProcessKind,
    pid: i32,
}

impl ReceivedProcess {
    pub fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        let target = fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
        let target = target.to_string_lossy();
        if target == "anon_inode:[pidfd]" {
            let pid = pidfd_pid(&fd)?;
            if pid <= 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "received pidfd names no live process",
                ));
            }
            verify_live_pidfd(&fd, pid)?;
            return Ok(Self {
                fd,
                kind: PeerProcessKind::PidFdOpen,
                pid,
            });
        }
        if let Some(pid) = target
            .strip_prefix("/proc/")
            .and_then(|rest| rest.parse::<i32>().ok())
            .filter(|pid| *pid > 0)
        {
            verify_proc_dir(&fd, pid)?;
            return Ok(Self {
                fd,
                kind: PeerProcessKind::ProcDir,
                pid,
            });
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("received descriptor is not a process handle: {target}"),
        ))
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    pub fn kind(&self) -> PeerProcessKind {
        self.kind
    }

    /// The process's cgroup document, read while the handle proves the process is still the
    /// one it was opened on.
    pub fn cgroup(&self) -> io::Result<String> {
        match self.kind {
            PeerProcessKind::SocketPidFd | PeerProcessKind::PidFdOpen => {
                pidfd_cgroup(&self.fd, self.pid)
            }
            PeerProcessKind::ProcDir => proc_dir_cgroup(&self.fd, self.pid),
        }
    }

    /// The app id of the `pf-app@<id>.service` unit this process runs in. A process in any other
    /// unit is not an app and is refused.
    pub fn app_id(&self) -> io::Result<String> {
        let cgroup = self.cgroup()?;
        service_unit_from_cgroup(&cgroup)
            .as_deref()
            .and_then(app_id_from_unit)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("pid={} is not in a pf-app@ unit", self.pid),
                )
            })
    }
}

/// `SCM_RIGHTS` transport for a process handle alongside a request payload.
pub mod scm {
    use std::io;
    use std::os::fd::{BorrowedFd, FromRawFd, OwnedFd, RawFd};

    /// Send `data` plus at most one fd as a single `sendmsg`. `data` must be non-empty so the
    /// ancillary data has a byte to ride on.
    pub fn send_with_fd(sock: RawFd, data: &[u8], fd: Option<BorrowedFd<'_>>) -> io::Result<()> {
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
        let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
        let mut cbuf = vec![0u8; space];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if let Some(fd) = fd {
            msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = space as _;
            // SAFETY: msg is initialized; cmsg pointers come from CMSG_FIRSTHDR over our buffer.
            unsafe {
                let cmsg = libc::CMSG_FIRSTHDR(&msg);
                if cmsg.is_null() {
                    return Err(io::Error::other("no cmsg header"));
                }
                (*cmsg).cmsg_level = libc::SOL_SOCKET;
                (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
                let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);
                std::ptr::copy_nonoverlapping(&raw, libc::CMSG_DATA(cmsg) as *mut RawFd, 1);
            }
        }
        // SAFETY: msg, iov and the control buffer are valid for the call.
        let sent = unsafe { libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL) };
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

    /// Receive up to `data.len()` payload bytes plus at most one fd. Surplus fds a hostile peer
    /// attaches are closed, never kept.
    pub fn recv_with_fd(sock: RawFd, data: &mut [u8]) -> io::Result<(usize, Option<OwnedFd>)> {
        let mut iov = libc::iovec {
            iov_base: data.as_mut_ptr() as *mut libc::c_void,
            iov_len: data.len(),
        };
        // Room for several fds so a flood is received (and closed) rather than truncated.
        let space = unsafe { libc::CMSG_SPACE(8 * std::mem::size_of::<RawFd>() as u32) } as usize;
        let mut cbuf = vec![0u8; space];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = space as _;
        // SAFETY: msg and buffers are valid; cmsgs are walked only within the returned length.
        let received = unsafe { libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if received < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut first: Option<OwnedFd> = None;
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS
                {
                    let data_ptr = libc::CMSG_DATA(cmsg) as *const RawFd;
                    let payload = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                    for index in 0..payload / std::mem::size_of::<RawFd>() {
                        let raw = std::ptr::read_unaligned(data_ptr.add(index));
                        let owned = OwnedFd::from_raw_fd(raw);
                        if first.is_none() {
                            first = Some(owned);
                        }
                    }
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
        }
        if msg.msg_flags & libc::MSG_CTRUNC != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ancillary data truncated",
            ));
        }
        Ok((received as usize, first))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;
    use std::process::{Command, Stdio};

    #[test]
    fn service_unit_and_app_id_come_only_from_the_systemd_hierarchy() {
        assert_eq!(
            service_unit_from_cgroup("0::/system.slice/pf-app@org.example.game.service\n")
                .as_deref(),
            Some("pf-app@org.example.game.service")
        );
        assert_eq!(
            service_unit_from_cgroup("1:name=systemd:/system.slice/pf-settings.service\n")
                .as_deref(),
            Some("pf-settings.service")
        );
        assert_eq!(
            service_unit_from_cgroup("5:freezer:/pf-settings.service\n"),
            None
        );
        assert_eq!(
            service_unit_from_cgroup("0::/user.slice/user-1000.slice/session-3.scope\n"),
            None
        );
        assert_eq!(
            app_id_from_unit("pf-app@org.example.game.service").as_deref(),
            Some("org.example.game")
        );
        for unit in [
            "pf-app@.service",
            "pf-app@org.example.game",
            "pf-settings.service",
            "not-pf-app@x.service",
            "pf-app@bad/id.service",
        ] {
            assert_eq!(app_id_from_unit(unit), None, "{unit:?}");
        }
        assert!(unit_matches(
            "0::/system.slice/pf-input-broker.service\n",
            "pf-input-broker.service"
        ));
        assert!(!unit_matches(
            "0::/system.slice/pf-input-broker.service\n",
            "pf-app@pf-input-broker.service"
        ));
    }

    #[test]
    fn received_pidfd_reads_a_live_child_and_fails_closed_once_it_exits() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let expected = fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();

        let received = ReceivedProcess::from_fd(pidfd_open(pid).unwrap()).unwrap();
        assert_eq!(received.kind(), PeerProcessKind::PidFdOpen);
        assert_eq!(received.pid(), pid);
        assert_eq!(received.cgroup().unwrap(), expected);

        child.kill().unwrap();
        child.wait().unwrap();
        let error = received.cgroup().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
        let error = ReceivedProcess::from_fd(pidfd_open(std::process::id() as i32).unwrap())
            .unwrap()
            .app_id()
            .unwrap_err();
        assert_eq!(
            error.kind(),
            io::ErrorKind::PermissionDenied,
            "a test process is not in a pf-app@ unit: {error}"
        );
    }

    #[test]
    fn received_proc_directory_is_pinned_and_other_descriptors_are_rejected() {
        let pid = std::process::id() as i32;
        let received = ReceivedProcess::from_fd(proc_dir_open(pid).unwrap()).unwrap();
        assert_eq!(received.kind(), PeerProcessKind::ProcDir);
        assert_eq!(received.pid(), pid);
        assert_eq!(
            received.cgroup().unwrap(),
            fs::read_to_string("/proc/self/cgroup").unwrap()
        );

        let regular: OwnedFd = File::open("/dev/null").unwrap().into();
        let error = ReceivedProcess::from_fd(regular).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error}");
        let directory: OwnedFd = File::open("/proc").unwrap().into();
        assert!(ReceivedProcess::from_fd(directory).is_err());
    }

    #[test]
    fn scm_rights_carries_one_process_handle_with_the_payload() {
        let (client, server) = UnixStream::pair().unwrap();
        let pidfd = pidfd_open(std::process::id() as i32).unwrap();
        scm::send_with_fd(client.as_raw_fd(), b"hello", Some(pidfd.as_fd())).unwrap();
        let mut buffer = [0u8; 16];
        let (len, fd) = scm::recv_with_fd(server.as_raw_fd(), &mut buffer).unwrap();
        assert_eq!(&buffer[..len], b"hello");
        let received = ReceivedProcess::from_fd(fd.expect("fd rode along")).unwrap();
        assert_eq!(received.pid(), std::process::id() as i32);

        scm::send_with_fd(client.as_raw_fd(), b"bare", None).unwrap();
        let (len, fd) = scm::recv_with_fd(server.as_raw_fd(), &mut buffer).unwrap();
        assert_eq!(&buffer[..len], b"bare");
        assert!(fd.is_none());
        assert!(scm::send_with_fd(client.as_raw_fd(), b"", None).is_err());
    }

    #[test]
    fn socket_peer_pidfd_stabilizes_the_proc_cgroup_lookup() {
        let (peer, _other) = UnixStream::pair().unwrap();
        let cred = peer_cred(&peer).unwrap();
        assert_eq!(cred.pid, std::process::id() as i32);
        assert_eq!(cred.uid, unsafe { libc::geteuid() });
        let source = KernelPeerProcessSource;
        let process = acquire_peer_process(&peer, cred.pid, &source).unwrap();
        assert_eq!(process.kind, PeerProcessKind::SocketPidFd);
        assert_eq!(
            source.cgroup(&process, cred.pid).unwrap(),
            fs::read_to_string("/proc/self/cgroup").unwrap()
        );
        // The authenticated-service check: this process is not in the expected unit.
        let error = verify_peer_service(&peer, cred.uid, "pf-input-broker.service").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        let error = verify_peer_service(&peer, cred.uid.wrapping_add(1), "x.service").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
}
