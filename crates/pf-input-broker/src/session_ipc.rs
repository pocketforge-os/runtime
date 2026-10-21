//! Transport and accept-time admission shared by the session input endpoints.
//!
//! The frozen input IPC contract keeps serialization in `pf-input-ipc`; this module owns the
//! Linux boundary: socket-derived identity, PFW1 framing, frame limits, and descriptor transfer.

use std::ffi::CString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use pf_input_ipc::compositor::{
    CompositorSession, CompositorStateError, CompositorToRouter, RouterToCompositor,
    TrustedConnectionOutcome,
};
use pf_input_ipc::Role;

use crate::router::SessionSignals;

struct CompositorState {
    current_epoch: Option<u64>,
    session: Option<CompositorSession>,
    signals: SessionSignals,
}

/// Session state driven only by the currently registered compositor connection.
pub struct CompositorAuthority {
    next_epoch: AtomicU64,
    state: Mutex<CompositorState>,
}

impl Default for CompositorAuthority {
    fn default() -> Self {
        Self {
            next_epoch: AtomicU64::new(1),
            state: Mutex::new(CompositorState {
                current_epoch: None,
                session: None,
                signals: SessionSignals::default(),
            }),
        }
    }
}

impl CompositorAuthority {
    fn register(&self) -> u64 {
        let epoch = self.next_epoch.fetch_add(1, Ordering::Relaxed).max(1);
        let mut state = self.state.lock().expect("compositor state poisoned");
        state.current_epoch = Some(epoch);
        state.session = Some(CompositorSession::new(epoch));
        state.signals.register_compositor(epoch);
        epoch
    }

    fn apply(&self, connection_epoch: u64, message: CompositorToRouter) -> io::Result<()> {
        let mut state = self.state.lock().expect("compositor state poisoned");
        if state.current_epoch != Some(connection_epoch) {
            return Err(invalid("superseded compositor connection"));
        }
        let session = state.session.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "compositor is not registered")
        })?;
        match message {
            CompositorToRouter::Register => {
                return Err(invalid("duplicate compositor register"));
            }
            CompositorToRouter::StateSnapshot { snapshot } => {
                let epoch = snapshot.epoch;
                let secure_entry = snapshot.focus.secure_entry;
                session.apply_snapshot(snapshot).map_err(state_error)?;
                state.signals.snapshot(epoch, secure_entry);
            }
            CompositorToRouter::Focus {
                epoch,
                secure_entry,
                ..
            } => {
                session.accept_delta(epoch).map_err(state_error)?;
                state.signals.focus(epoch, secure_entry);
            }
            CompositorToRouter::Overlay { epoch, .. } => {
                session.accept_delta(epoch).map_err(state_error)?;
            }
            CompositorToRouter::PhysicalInput { epoch, class } => {
                session.accept_delta(epoch).map_err(state_error)?;
                state.signals.physical_input(epoch, class);
            }
            CompositorToRouter::ProtectedPower {
                epoch,
                sequence,
                state: power,
            } => {
                session
                    .protected_power(epoch, sequence, power)
                    .map_err(state_error)?;
            }
            CompositorToRouter::TrustedConnectionResult {
                epoch,
                request_id,
                result,
            } => match result {
                TrustedConnectionOutcome::Ok => session
                    .finish_trusted_connection(epoch, request_id, 0)
                    .map_err(state_error)?,
                TrustedConnectionOutcome::Refused { .. } => session
                    .refuse_trusted_connection(epoch, request_id, 0)
                    .map_err(state_error)?,
            },
            CompositorToRouter::Pong { epoch, .. } => {
                session.accept_delta(epoch).map_err(state_error)?;
            }
        }
        Ok(())
    }

    fn disconnect(&self, epoch: u64) {
        let mut state = self.state.lock().expect("compositor state poisoned");
        if state.current_epoch != Some(epoch) {
            return;
        }
        if let Some(session) = state.session.as_mut() {
            session.disconnect(false);
        }
        state.current_epoch = None;
        state.session = None;
        state.signals.disconnect();
    }

    pub fn snapshot_ready(&self, epoch: u64) -> bool {
        self.state
            .lock()
            .expect("compositor state poisoned")
            .signals
            .snapshot_ready(epoch)
    }

    pub fn secure_entry(&self) -> bool {
        self.state
            .lock()
            .expect("compositor state poisoned")
            .signals
            .secure_entry()
    }
}

/// Serve one already-authenticated compositor. Registration is the mandatory first frame and
/// each new connection receives a fresh epoch; the complete snapshot must precede every delta.
pub fn serve_compositor_peer(
    mut stream: UnixStream,
    authority: &CompositorAuthority,
) -> io::Result<()> {
    let (body, fd) = read_frame_with_fd(&mut stream)?;
    if fd.is_some() {
        return Err(invalid("compositor register carried a descriptor"));
    }
    let first: CompositorToRouter = pf_input_ipc::decode(&body).map_err(ipc_error)?;
    if first != CompositorToRouter::Register {
        return Err(invalid("compositor must register before state replay"));
    }
    let epoch = authority.register();
    write_message(&mut stream, &RouterToCompositor::Registered { epoch })?;

    loop {
        let (body, fd) = match read_frame_with_fd(&mut stream) {
            Ok(frame) => frame,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                authority.disconnect(epoch);
                return Ok(());
            }
            Err(error) => {
                authority.disconnect(epoch);
                return Err(error);
            }
        };
        let message: CompositorToRouter = pf_input_ipc::decode(&body).map_err(ipc_error)?;
        let expected_fd = matches!(
            message,
            CompositorToRouter::TrustedConnectionResult {
                result: TrustedConnectionOutcome::Ok,
                ..
            }
        );
        if fd.is_some() != expected_fd {
            authority.disconnect(epoch);
            return Err(invalid(
                "compositor frame carried the wrong descriptor count",
            ));
        }
        // Trusted connection requests are added when the keyboard/menu listeners mint them. Until
        // then, an unsolicited result is rejected by CompositorSession as an unexpected id.
        let result = authority.apply(epoch, message);
        drop(fd);
        if let Err(error) = result {
            authority.disconnect(epoch);
            return Err(error);
        }
    }
}

/// Facts derived from the accepted socket and the peer's pinned process, never from a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerFacts {
    pub uid: u32,
    pub groups: Vec<u32>,
    pub unit: String,
}

/// Exact identity admitted on one role-specific endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndpointPolicy {
    pub role: Role,
    pub session_uid: u32,
    pub role_gid: u32,
    pub unit: &'static str,
}

impl EndpointPolicy {
    /// Resolve the contract's group name and exact service unit for `role`.
    pub fn for_role(role: Role, session_uid: u32) -> io::Result<Self> {
        let (group, unit) = match role {
            Role::Compositor => (
                pf_input_ipc::sockets::COMPOSITOR_GROUP,
                "pocketforge-gamescope.service",
            ),
            Role::Keyboard => (pf_input_ipc::sockets::KEYBOARD_GROUP, "pf-osk.service"),
            Role::Menu => (pf_input_ipc::sockets::MENU_GROUP, "pf-system-menu.service"),
        };
        Ok(Self {
            role,
            session_uid,
            role_gid: group_gid(group)?,
            unit,
        })
    }
}

/// Resolve a configured session user without trusting environment-provided numeric ids.
pub fn user_uid(name: &str) -> io::Result<u32> {
    let name =
        CString::new(name).map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    // SAFETY: `name` is NUL terminated. `getpwnam_r` writes only through supplied buffers.
    unsafe {
        let mut user: libc::passwd = std::mem::zeroed();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; 16 * 1024];
        let rc = libc::getpwnam_r(
            name.as_ptr(),
            &mut user,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        );
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
        if result.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "session user not found",
            ));
        }
        Ok(user.pw_uid)
    }
}

/// Bind one role endpoint with the frozen `0660 root:<role-group>` ownership. Only a stale socket
/// is replaced; a regular file, directory, or symlink collision fails closed.
pub fn bind_endpoint(path: &Path, policy: &EndpointPolicy) -> io::Result<UnixListener> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(path)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("refusing to replace non-socket endpoint {}", path.display()),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let listener = UnixListener::bind(path)?;
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    // SAFETY: c_path is a NUL-terminated path to the socket just created in the protected run dir.
    let rc = unsafe { libc::chown(c_path.as_ptr(), libc::uid_t::MAX, policy.role_gid) };
    if rc < 0 {
        return Err(cleanup_bound_socket(path, io::Error::last_os_error()));
    }
    if let Err(error) = fs::set_permissions(
        path,
        fs::Permissions::from_mode(pf_input_ipc::sockets::SOCKET_MODE),
    ) {
        return Err(cleanup_bound_socket(path, error));
    }
    if let Err(error) = listener.set_nonblocking(true) {
        return Err(cleanup_bound_socket(path, error));
    }
    Ok(listener)
}

/// Apply all three independent endpoint checks. A matching uid alone is never authority because
/// apps and trusted surfaces deliberately share the session uid.
pub fn authorize_peer(policy: &EndpointPolicy, facts: &PeerFacts) -> io::Result<Role> {
    if facts.uid != policy.session_uid {
        return Err(denied(format!(
            "role {:?} peer uid {} does not match session uid {}",
            policy.role, facts.uid, policy.session_uid
        )));
    }
    if !facts.groups.contains(&policy.role_gid) {
        return Err(denied(format!(
            "role {:?} peer lacks endpoint group {}",
            policy.role, policy.role_gid
        )));
    }
    if facts.unit != policy.unit {
        return Err(denied(format!(
            "role {:?} peer unit {:?} does not match {:?}",
            policy.role, facts.unit, policy.unit
        )));
    }
    Ok(policy.role)
}

/// Derive and authorize the identity behind an accepted Unix stream.
pub fn authenticate_peer(stream: &UnixStream, policy: &EndpointPolicy) -> io::Result<Role> {
    let identity = pf_peer_identity::peer_service_identity(stream)?;
    let facts = PeerFacts {
        uid: identity.cred.uid,
        groups: pf_peer_identity::socket_peer_groups(stream)?,
        unit: identity.unit,
    };
    authorize_peer(policy, &facts)
}

/// Send one bounded PFW1 frame, attaching exactly one descriptor to its first byte.
pub fn write_frame_with_fd(stream: &mut UnixStream, body: &[u8], fd: RawFd) -> io::Result<()> {
    if body.len() > pf_input_ipc::MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "input IPC frame exceeds 64 KiB",
        ));
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(body);
    let sent = send_first_with_fd(stream.as_raw_fd(), &frame, fd)?;
    stream.write_all(&frame[sent..])
}

/// Receive one bounded PFW1 frame and its optional single descriptor. More than one descriptor
/// is a protocol error; every received descriptor is closed on that path.
pub fn read_frame_with_fd(stream: &mut UnixStream) -> io::Result<(Vec<u8>, Option<OwnedFd>)> {
    let (first, fd) = recv_first_with_fd(stream.as_raw_fd())?;
    let mut prefix = [0_u8; 4];
    prefix[0] = first;
    stream.read_exact(&mut prefix[1..])?;
    let len = u32::from_be_bytes(prefix) as usize;
    if len > pf_input_ipc::MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "input IPC frame exceeds 64 KiB",
        ));
    }
    let mut body = vec![0_u8; len];
    stream.read_exact(&mut body)?;
    Ok((body, fd))
}

fn denied(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

fn cleanup_bound_socket(path: &Path, cause: io::Error) -> io::Error {
    match fs::remove_file(path) {
        Ok(()) => cause,
        Err(cleanup) => io::Error::new(
            cause.kind(),
            format!(
                "{cause}; additionally failed to remove {}: {cleanup}",
                path.display()
            ),
        ),
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn ipc_error(error: pf_input_ipc::IpcError) -> io::Error {
    invalid(error.to_string())
}

fn state_error(error: CompositorStateError) -> io::Error {
    invalid(format!("invalid compositor state transition: {error:?}"))
}

fn write_message<M: serde::Serialize>(stream: &mut UnixStream, message: &M) -> io::Result<()> {
    let body = pf_input_ipc::encode(message);
    pf_wire::write_frame(stream, &body)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
}

fn group_gid(name: &str) -> io::Result<u32> {
    let name =
        CString::new(name).map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    // SAFETY: `name` is NUL terminated. `getgrnam_r` writes only through the supplied buffers.
    unsafe {
        let mut group: libc::group = std::mem::zeroed();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; 16 * 1024];
        let rc = libc::getgrnam_r(
            name.as_ptr(),
            &mut group,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        );
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
        if result.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "endpoint group not found",
            ));
        }
        Ok(group.gr_gid)
    }
}

fn send_first_with_fd(sock: RawFd, bytes: &[u8], fd: RawFd) -> io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0_u8; space];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len();
    // SAFETY: every pointer in `message` references live storage for the duration of sendmsg.
    let sent = unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&message);
        if cmsg.is_null() {
            return Err(io::Error::other(
                "could not allocate descriptor control header",
            ));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as usize;
        std::ptr::copy_nonoverlapping(&fd, libc::CMSG_DATA(cmsg).cast::<RawFd>(), 1);
        libc::sendmsg(sock, &message, libc::MSG_NOSIGNAL)
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    if sent == 0 {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "socket wrote no frame bytes",
        ));
    }
    Ok(sent as usize)
}

fn recv_first_with_fd(sock: RawFd) -> io::Result<(u8, Option<OwnedFd>)> {
    let mut first = 0_u8;
    let mut iov = libc::iovec {
        iov_base: (&mut first as *mut u8).cast(),
        iov_len: 1,
    };
    const MAX_FDS: usize = 8;
    let space =
        unsafe { libc::CMSG_SPACE((MAX_FDS * std::mem::size_of::<RawFd>()) as u32) } as usize;
    let mut control = vec![0_u8; space];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len();
    // SAFETY: every pointer in `message` references writable live storage for recvmsg.
    let received = unsafe { libc::recvmsg(sock, &mut message, libc::MSG_CMSG_CLOEXEC) };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }
    if received == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "input IPC peer closed",
        ));
    }
    if message.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "input IPC ancillary data was truncated",
        ));
    }

    let mut fds = Vec::new();
    // SAFETY: libc's cmsg traversal is bounded by recvmsg's returned `msg_controllen`.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&message);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let bytes = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let count = bytes / std::mem::size_of::<RawFd>();
                let values = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                for index in 0..count {
                    fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(
                        values.add(index),
                    )));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&message, cmsg);
        }
    }
    if fds.len() > 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "input IPC frame carried more than one descriptor",
        ));
    }
    Ok((first, fds.pop()))
}
