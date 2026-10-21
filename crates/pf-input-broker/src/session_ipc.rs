//! Transport and accept-time admission shared by the session input endpoints.
//!
//! The frozen input IPC contract keeps serialization in `pf-input-ipc`; this module owns the
//! Linux boundary: socket-derived identity, PFW1 framing, frame limits, and descriptor transfer.

use std::collections::{BTreeMap, HashMap};
use std::ffi::CString;
use std::fs;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Mutex;

use pf_input_ipc::compositor::{
    CompositorSession, CompositorStateError, CompositorToRouter, RouterToCompositor,
    TrustedConnectionOutcome,
};
use pf_input_ipc::keyboard::{KeyboardToRouter, RouterToKeyboard};
use pf_input_ipc::menu::{MenuToRouter, RouterToMenu};
use pf_input_ipc::{OverlayRole, Role};

use crate::router::SessionSignals;

struct CompositorState {
    current_epoch: Option<u64>,
    session: Option<CompositorSession>,
    signals: SessionSignals,
    writer: Option<UnixStream>,
    pending_fds: HashMap<u64, SyncSender<io::Result<OwnedFd>>>,
    role_peers: BTreeMap<OverlayRole, RolePeer>,
}

struct RolePeer {
    control: UnixStream,
    pad: OwnedFd,
    wayland: OwnedFd,
}

impl RolePeer {
    fn invalidate(self) {
        // SCM_RIGHTS duplicates share the same open file description. Revoking through the
        // retained evdev fd therefore invalidates the role's delivered pad descriptor too.
        unsafe {
            libc::ioctl(self.pad.as_raw_fd(), crate::ioc::EVIOCREVOKE as _, 0);
            libc::shutdown(self.pad.as_raw_fd(), libc::SHUT_RDWR);
            libc::shutdown(self.wayland.as_raw_fd(), libc::SHUT_RDWR);
        }
        let _ = self.control.shutdown(Shutdown::Both);
    }
}

/// Session state driven only by the currently registered compositor connection.
pub struct CompositorAuthority {
    next_epoch: AtomicU64,
    next_request: AtomicU64,
    state: Mutex<CompositorState>,
}

impl Default for CompositorAuthority {
    fn default() -> Self {
        Self {
            next_epoch: AtomicU64::new(1),
            next_request: AtomicU64::new(1),
            state: Mutex::new(CompositorState {
                current_epoch: None,
                session: None,
                signals: SessionSignals::default(),
                writer: None,
                pending_fds: HashMap::new(),
                role_peers: BTreeMap::new(),
            }),
        }
    }
}

impl CompositorAuthority {
    fn register(&self, writer: UnixStream) -> u64 {
        let epoch = self.next_epoch.fetch_add(1, Ordering::Relaxed).max(1);
        let mut state = self.state.lock().expect("compositor state poisoned");
        for peer in std::mem::take(&mut state.role_peers).into_values() {
            peer.invalidate();
        }
        state.pending_fds.clear();
        state.current_epoch = Some(epoch);
        state.session = Some(CompositorSession::new(epoch));
        state.signals.register_compositor(epoch);
        state.writer = Some(writer);
        epoch
    }

    fn apply(
        &self,
        connection_epoch: u64,
        message: CompositorToRouter,
        fd: Option<OwnedFd>,
    ) -> io::Result<()> {
        let mut state = self.state.lock().expect("compositor state poisoned");
        if state.current_epoch != Some(connection_epoch) {
            return Err(invalid("superseded compositor connection"));
        }
        if !matches!(message, CompositorToRouter::TrustedConnectionResult { .. }) && fd.is_some() {
            return Err(invalid("compositor state frame carried a descriptor"));
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
                TrustedConnectionOutcome::Ok => {
                    session
                        .finish_trusted_connection(epoch, request_id, usize::from(fd.is_some()))
                        .map_err(state_error)?;
                    let sender = state.pending_fds.remove(&request_id).ok_or_else(|| {
                        invalid("trusted connection result had no pending receiver")
                    })?;
                    let descriptor = fd.expect("CompositorSession checked the descriptor");
                    let _ = sender.send(Ok(descriptor));
                }
                TrustedConnectionOutcome::Refused { reason } => {
                    session
                        .refuse_trusted_connection(epoch, request_id, usize::from(fd.is_some()))
                        .map_err(state_error)?;
                    let sender = state.pending_fds.remove(&request_id).ok_or_else(|| {
                        invalid("trusted connection refusal had no pending receiver")
                    })?;
                    let _ =
                        sender.send(Err(io::Error::new(io::ErrorKind::PermissionDenied, reason)));
                }
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
        state.writer = None;
        state.pending_fds.clear();
        for peer in std::mem::take(&mut state.role_peers).into_values() {
            peer.invalidate();
        }
        state.signals.disconnect();
    }

    /// Ask the active compositor to mint one role-bound Wayland connection. The returned
    /// receiver is invalidated if the compositor epoch changes before its descriptor arrives.
    pub fn request_trusted_connection(
        &self,
        role: OverlayRole,
    ) -> io::Result<Receiver<io::Result<OwnedFd>>> {
        self.request_trusted_connection_with_epoch(role)
            .map(|(_, receiver)| receiver)
    }

    fn request_trusted_connection_with_epoch(
        &self,
        role: OverlayRole,
    ) -> io::Result<(u64, Receiver<io::Result<OwnedFd>>)> {
        let unit = match role {
            OverlayRole::Keyboard => "pf-osk.service",
            OverlayRole::Menu => "pf-system-menu.service",
            OverlayRole::Toast => return Err(invalid("toast has no role endpoint")),
        };
        let request_id = self.next_request.fetch_add(1, Ordering::Relaxed).max(1);
        let mut state = self.state.lock().expect("compositor state poisoned");
        let epoch = state.current_epoch.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "compositor is not registered")
        })?;
        state
            .session
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "compositor is absent"))?
            .begin_trusted_connection(epoch, request_id, role)
            .map_err(state_error)?;
        let message = RouterToCompositor::TrustedConnection {
            epoch,
            request_id,
            role,
            unit: unit.into(),
        };
        let (sender, receiver) = mpsc::sync_channel(1);
        state.pending_fds.insert(request_id, sender);
        let write_result = state
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "compositor writer absent"))
            .and_then(|writer| write_message(writer, &message));
        if let Err(error) = write_result {
            state.pending_fds.remove(&request_id);
            return Err(error);
        }
        Ok((epoch, receiver))
    }

    fn register_role_peer(&self, epoch: u64, role: OverlayRole, peer: RolePeer) -> io::Result<()> {
        let mut state = self.state.lock().expect("compositor state poisoned");
        if state.current_epoch != Some(epoch) {
            peer.invalidate();
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "compositor epoch changed while registering role peer",
            ));
        }
        if state.role_peers.contains_key(&role) {
            peer.invalidate();
            return Err(invalid("trusted role peer was already registered"));
        }
        state.role_peers.insert(role, peer);
        Ok(())
    }

    fn release_trusted_connection(&self, epoch: u64, role: OverlayRole) {
        let mut state = self.state.lock().expect("compositor state poisoned");
        if state.current_epoch != Some(epoch) {
            return;
        }
        if let Some(peer) = state.role_peers.remove(&role) {
            peer.invalidate();
        }
        if let Some(session) = state.session.as_mut() {
            let _ = session.release_trusted_role(epoch, role);
        }
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
    let epoch = authority.register(stream.try_clone()?);
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
        // Trusted connection requests are added when the keyboard/menu listeners mint them. Until
        // then, an unsolicited result is rejected by CompositorSession as an unexpected id.
        let result = authority.apply(epoch, message, fd);
        if let Err(error) = result {
            authority.disconnect(epoch);
            return Err(error);
        }
    }
}

/// Register one already-authenticated system-menu peer and deliver its private pad fd followed by
/// a Wayland fd minted for the current compositor epoch. No partial registration is ever sent.
pub fn serve_menu_peer(
    mut stream: UnixStream,
    authority: &CompositorAuthority,
    pad: OwnedFd,
) -> io::Result<()> {
    let (body, fd) = read_frame_with_fd(&mut stream)?;
    if fd.is_some() {
        return Err(invalid("menu register carried a descriptor"));
    }
    let first: MenuToRouter = pf_input_ipc::decode(&body).map_err(ipc_error)?;
    if first != MenuToRouter::Register {
        return Err(invalid("menu must register before other messages"));
    }

    let (epoch, trusted) = authority.request_trusted_connection_with_epoch(OverlayRole::Menu)?;
    let trusted = trusted.recv().map_err(|_| {
        io::Error::new(io::ErrorKind::ConnectionAborted, "compositor epoch changed")
    })??;
    let role_control = match stream.try_clone() {
        Ok(control) => control,
        Err(error) => {
            authority.release_trusted_connection(epoch, OverlayRole::Menu);
            return Err(error);
        }
    };
    let registered = pf_input_ipc::encode(&RouterToMenu::Registered);
    if let Err(error) = write_frame_with_fds(
        &mut stream,
        &registered,
        &[pad.as_raw_fd(), trusted.as_raw_fd()],
    ) {
        authority.release_trusted_connection(epoch, OverlayRole::Menu);
        return Err(error);
    }
    authority.register_role_peer(
        epoch,
        OverlayRole::Menu,
        RolePeer {
            control: role_control,
            pad,
            wayland: trusted,
        },
    )?;

    loop {
        let (body, fd) = match read_frame_with_fd(&mut stream) {
            Ok(frame) => frame,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                authority.release_trusted_connection(epoch, OverlayRole::Menu);
                return Ok(());
            }
            Err(error) => {
                authority.release_trusted_connection(epoch, OverlayRole::Menu);
                return Err(error);
            }
        };
        if fd.is_some() {
            authority.release_trusted_connection(epoch, OverlayRole::Menu);
            return Err(invalid("menu message carried a descriptor"));
        }
        let message: MenuToRouter = match pf_input_ipc::decode(&body).map_err(ipc_error) {
            Ok(message) => message,
            Err(error) => {
                authority.release_trusted_connection(epoch, OverlayRole::Menu);
                return Err(error);
            }
        };
        if message == MenuToRouter::Register {
            authority.release_trusted_connection(epoch, OverlayRole::Menu);
            return Err(invalid("duplicate menu register"));
        }
        // Menu policy transitions are consumed by the session router worker when this transport
        // is wired into the daemon; this loop already enforces framing and descriptor authority.
    }
}

/// Register one already-authenticated keyboard peer and deliver its private pad fd followed by a
/// Wayland fd minted for the current compositor epoch. No partial registration is ever sent.
pub fn serve_keyboard_peer(
    mut stream: UnixStream,
    authority: &CompositorAuthority,
    pad: OwnedFd,
) -> io::Result<()> {
    let (body, fd) = read_frame_with_fd(&mut stream)?;
    if fd.is_some() {
        return Err(invalid("keyboard register carried a descriptor"));
    }
    let first: KeyboardToRouter = pf_input_ipc::decode(&body).map_err(ipc_error)?;
    if !matches!(first, KeyboardToRouter::Register { .. }) {
        return Err(invalid("keyboard must register before other messages"));
    }

    let (epoch, trusted) =
        authority.request_trusted_connection_with_epoch(OverlayRole::Keyboard)?;
    let trusted = trusted.recv().map_err(|_| {
        io::Error::new(io::ErrorKind::ConnectionAborted, "compositor epoch changed")
    })??;
    let role_control = match stream.try_clone() {
        Ok(control) => control,
        Err(error) => {
            authority.release_trusted_connection(epoch, OverlayRole::Keyboard);
            return Err(error);
        }
    };
    let registered = pf_input_ipc::encode(&RouterToKeyboard::Registered);
    if let Err(error) = write_frame_with_fds(
        &mut stream,
        &registered,
        &[pad.as_raw_fd(), trusted.as_raw_fd()],
    ) {
        authority.release_trusted_connection(epoch, OverlayRole::Keyboard);
        return Err(error);
    }
    authority.register_role_peer(
        epoch,
        OverlayRole::Keyboard,
        RolePeer {
            control: role_control,
            pad,
            wayland: trusted,
        },
    )?;

    loop {
        let (body, fd) = match read_frame_with_fd(&mut stream) {
            Ok(frame) => frame,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                authority.release_trusted_connection(epoch, OverlayRole::Keyboard);
                return Ok(());
            }
            Err(error) => {
                authority.release_trusted_connection(epoch, OverlayRole::Keyboard);
                return Err(error);
            }
        };
        if fd.is_some() {
            authority.release_trusted_connection(epoch, OverlayRole::Keyboard);
            return Err(invalid("keyboard message carried a descriptor"));
        }
        let message: KeyboardToRouter = match pf_input_ipc::decode(&body).map_err(ipc_error) {
            Ok(message) => message,
            Err(error) => {
                authority.release_trusted_connection(epoch, OverlayRole::Keyboard);
                return Err(error);
            }
        };
        if matches!(message, KeyboardToRouter::Register { .. }) {
            authority.release_trusted_connection(epoch, OverlayRole::Keyboard);
            return Err(invalid("duplicate keyboard register"));
        }
        // Keyboard policy transitions are consumed by the session router worker when this
        // transport is wired into the daemon; this loop already enforces framing and descriptor
        // authority.
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
    /// `None` means the role's service unit is not installed and every peer is refused.
    pub expected_uid: Option<u32>,
    pub role_gid: u32,
    pub unit: &'static str,
}

impl EndpointPolicy {
    /// Resolve the contract's group name and exact service unit for `role`.
    pub fn for_role(role: Role, expected_uid: Option<u32>) -> io::Result<Self> {
        let (group, unit) = match role {
            Role::Compositor => (
                pf_input_ipc::sockets::COMPOSITOR_GROUP,
                pf_input_ipc::sockets::COMPOSITOR_UNIT,
            ),
            Role::Keyboard => (
                pf_input_ipc::sockets::KEYBOARD_GROUP,
                pf_input_ipc::sockets::KEYBOARD_UNIT,
            ),
            Role::Menu => (
                pf_input_ipc::sockets::MENU_GROUP,
                pf_input_ipc::sockets::MENU_UNIT,
            ),
        };
        Ok(Self {
            role,
            expected_uid,
            role_gid: group_gid(group)?,
            unit,
        })
    }
}

/// Typed reasons a trusted endpoint refuses an accepted socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndpointAdmissionError {
    Identity(String),
    RoleUnavailable {
        role: Role,
    },
    UidMismatch {
        role: Role,
        got: u32,
        expected: u32,
    },
    GroupMismatch {
        role: Role,
        expected_gid: u32,
    },
    UnitMismatch {
        role: Role,
        got: String,
        expected: &'static str,
    },
}

impl std::fmt::Display for EndpointAdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Identity(message) => {
                write!(formatter, "could not derive peer identity: {message}")
            }
            Self::RoleUnavailable { role } => {
                write!(formatter, "role {role:?} service unit is unavailable")
            }
            Self::UidMismatch {
                role,
                got,
                expected,
            } => write!(
                formatter,
                "role {role:?} peer uid {got} does not match pinned uid {expected}"
            ),
            Self::GroupMismatch { role, expected_gid } => write!(
                formatter,
                "role {role:?} peer lacks endpoint group {expected_gid}"
            ),
            Self::UnitMismatch {
                role,
                got,
                expected,
            } => write!(
                formatter,
                "role {role:?} peer unit {got:?} does not match {expected:?}"
            ),
        }
    }
}

impl std::error::Error for EndpointAdmissionError {}

/// Startup configuration error for the pinned trusted-role identity map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoleUidMappingError {
    DuplicateUid { uid: u32, first: Role, second: Role },
}

impl std::fmt::Display for RoleUidMappingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateUid { uid, first, second } => write!(
                formatter,
                "trusted roles {first:?} and {second:?} resolve to the same uid {uid}"
            ),
        }
    }
}

impl std::error::Error for RoleUidMappingError {}

/// Refuse a startup configuration in which one service uid can claim two trusted roles.
pub fn validate_role_uid_mapping(mapping: &[(Role, u32)]) -> Result<(), RoleUidMappingError> {
    for (index, (first_role, first_uid)) in mapping.iter().enumerate() {
        for (second_role, second_uid) in &mapping[index + 1..] {
            if first_uid == second_uid && first_role != second_role {
                return Err(RoleUidMappingError::DuplicateUid {
                    uid: *first_uid,
                    first: *first_role,
                    second: *second_role,
                });
            }
        }
    }
    Ok(())
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

/// Apply all three independent endpoint checks. An unavailable role refuses every peer.
pub fn authorize_peer(
    policy: &EndpointPolicy,
    facts: &PeerFacts,
) -> Result<Role, EndpointAdmissionError> {
    let expected_uid = policy
        .expected_uid
        .ok_or(EndpointAdmissionError::RoleUnavailable { role: policy.role })?;
    if facts.uid != expected_uid {
        return Err(EndpointAdmissionError::UidMismatch {
            role: policy.role,
            got: facts.uid,
            expected: expected_uid,
        });
    }
    if !facts.groups.contains(&policy.role_gid) {
        return Err(EndpointAdmissionError::GroupMismatch {
            role: policy.role,
            expected_gid: policy.role_gid,
        });
    }
    if facts.unit != policy.unit {
        return Err(EndpointAdmissionError::UnitMismatch {
            role: policy.role,
            got: facts.unit.clone(),
            expected: policy.unit,
        });
    }
    Ok(policy.role)
}

/// Derive and authorize the identity behind an accepted Unix stream.
pub fn authenticate_peer(
    stream: &UnixStream,
    policy: &EndpointPolicy,
) -> Result<Role, EndpointAdmissionError> {
    let identity = pf_peer_identity::peer_service_identity(stream)
        .map_err(|error| EndpointAdmissionError::Identity(error.to_string()))?;
    let facts = PeerFacts {
        uid: identity.cred.uid,
        groups: pf_peer_identity::socket_peer_groups(stream)
            .map_err(|error| EndpointAdmissionError::Identity(error.to_string()))?,
        unit: identity.unit,
    };
    authorize_peer(policy, &facts)
}

/// Send one bounded PFW1 frame, attaching exactly one descriptor to its first byte.
pub fn write_frame_with_fd(stream: &mut UnixStream, body: &[u8], fd: RawFd) -> io::Result<()> {
    write_frame_with_fds(stream, body, &[fd])
}

/// Send one bounded PFW1 frame with every descriptor in a single `SCM_RIGHTS` message.
pub fn write_frame_with_fds(stream: &mut UnixStream, body: &[u8], fds: &[RawFd]) -> io::Result<()> {
    if body.len() > pf_input_ipc::MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "input IPC frame exceeds 64 KiB",
        ));
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(body);
    if fds.is_empty() {
        return stream.write_all(&frame);
    }
    let sent = send_first_with_fds(stream.as_raw_fd(), &frame, fds)?;
    stream.write_all(&frame[sent..])
}

/// Receive one bounded PFW1 frame and its optional single descriptor. More than one descriptor
/// is a protocol error; every received descriptor is closed on that path.
pub fn read_frame_with_fd(stream: &mut UnixStream) -> io::Result<(Vec<u8>, Option<OwnedFd>)> {
    let (body, mut fds) = read_frame_with_fds(stream).map_err(frame_receive_io)?;
    if fds.len() > 1 {
        return Err(invalid("input IPC frame carried more than one descriptor"));
    }
    Ok((body, fds.pop()))
}

/// The approved two-descriptor ABI returned by a keyboard/menu `registered` frame.
#[derive(Debug)]
pub struct RegisteredDescriptors {
    pub pad: OwnedFd,
    pub wayland: OwnedFd,
}

/// Typed fail-closed reasons for rejecting a keyboard/menu `registered` frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisteredFrameError {
    Transport(io::ErrorKind),
    ControlTruncated,
    DescriptorCount { got: usize },
    FirstNotEvdev,
    SecondNotWayland,
    NotCloseOnExec { index: usize },
}

impl std::fmt::Display for RegisteredFrameError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(kind) => {
                write!(formatter, "registered frame transport error: {kind:?}")
            }
            Self::ControlTruncated => {
                formatter.write_str("registered descriptor control truncated")
            }
            Self::DescriptorCount { got } => {
                write!(
                    formatter,
                    "registered carried {got} descriptors instead of two"
                )
            }
            Self::FirstNotEvdev => formatter.write_str("registered first descriptor is not evdev"),
            Self::SecondNotWayland => formatter
                .write_str("registered second descriptor is not a connected Wayland socket"),
            Self::NotCloseOnExec { index } => {
                write!(
                    formatter,
                    "registered descriptor {index} is not close-on-exec"
                )
            }
        }
    }
}

impl std::error::Error for RegisteredFrameError {}

/// Receive the approved `[private_pad_evdev_fd, trusted_wayland_client_fd]` descriptor pair.
/// `validate_pad` exists so hermetic tests can identify their synthetic stand-in; production uses
/// [`read_registered_frame`], which executes an evdev ioctl on the first descriptor.
pub fn read_registered_frame_with(
    stream: &mut UnixStream,
    validate_pad: impl FnOnce(BorrowedFd<'_>) -> io::Result<()>,
) -> Result<(Vec<u8>, RegisteredDescriptors), RegisteredFrameError> {
    let (body, fds) = read_frame_with_fds(stream).map_err(|error| match error {
        FrameReceiveError::Transport(error) => RegisteredFrameError::Transport(error.kind()),
        FrameReceiveError::ControlTruncated => RegisteredFrameError::ControlTruncated,
    })?;
    if fds.len() != 2 {
        return Err(RegisteredFrameError::DescriptorCount { got: fds.len() });
    }
    for (index, fd) in fds.iter().enumerate() {
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        if flags < 0 || flags & libc::FD_CLOEXEC == 0 {
            return Err(RegisteredFrameError::NotCloseOnExec { index });
        }
    }
    validate_pad(fds[0].as_fd()).map_err(|_| RegisteredFrameError::FirstNotEvdev)?;
    validate_wayland_client(fds[1].as_fd()).map_err(|_| RegisteredFrameError::SecondNotWayland)?;
    let mut fds = fds.into_iter();
    Ok((
        body,
        RegisteredDescriptors {
            pad: fds.next().expect("count checked"),
            wayland: fds.next().expect("count checked"),
        },
    ))
}

fn validate_wayland_client(fd: BorrowedFd<'_>) -> io::Result<()> {
    let mut socket_type = 0_i32;
    let mut socket_type_len = std::mem::size_of_val(&socket_type) as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            std::ptr::addr_of_mut!(socket_type).cast(),
            &mut socket_type_len,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    if socket_type != libc::SOCK_STREAM {
        return Err(invalid("trusted Wayland descriptor is not a stream socket"));
    }

    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let mut address_len = std::mem::size_of_val(&address) as libc::socklen_t;
    let rc = unsafe {
        libc::getpeername(
            fd.as_raw_fd(),
            std::ptr::addr_of_mut!(address).cast(),
            &mut address_len,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    if i32::from(address.sun_family) != libc::AF_UNIX {
        return Err(invalid("trusted Wayland descriptor is not AF_UNIX"));
    }
    Ok(())
}

/// Production `registered` receiver: the first descriptor must answer an evdev identity ioctl.
pub fn read_registered_frame(
    stream: &mut UnixStream,
) -> Result<(Vec<u8>, RegisteredDescriptors), RegisteredFrameError> {
    read_registered_frame_with(stream, |fd| {
        let mut identity: libc::input_id = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::ioctl(fd.as_raw_fd(), crate::ioc::eviocgid() as _, &mut identity) };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    })
}

#[derive(Debug)]
enum FrameReceiveError {
    Transport(io::Error),
    ControlTruncated,
}

fn frame_receive_io(error: FrameReceiveError) -> io::Error {
    match error {
        FrameReceiveError::Transport(error) => error,
        FrameReceiveError::ControlTruncated => invalid("input IPC ancillary data was truncated"),
    }
}

fn read_frame_with_fds(
    stream: &mut UnixStream,
) -> Result<(Vec<u8>, Vec<OwnedFd>), FrameReceiveError> {
    let (first, fds) = recv_first_with_fds(stream.as_raw_fd())?;
    let mut prefix = [0_u8; 4];
    prefix[0] = first;
    stream
        .read_exact(&mut prefix[1..])
        .map_err(FrameReceiveError::Transport)?;
    let len = u32::from_be_bytes(prefix) as usize;
    if len > pf_input_ipc::MAX_FRAME {
        return Err(FrameReceiveError::Transport(io::Error::new(
            io::ErrorKind::InvalidData,
            "input IPC frame exceeds 64 KiB",
        )));
    }
    let mut body = vec![0_u8; len];
    stream
        .read_exact(&mut body)
        .map_err(FrameReceiveError::Transport)?;
    Ok((body, fds))
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

fn send_first_with_fds(sock: RawFd, bytes: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let fd_bytes = std::mem::size_of_val(fds) as u32;
    let space = unsafe { libc::CMSG_SPACE(fd_bytes) } as usize;
    let mut control = vec![0_u8; space];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len() as _;
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
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_bytes) as _;
        std::ptr::copy_nonoverlapping(
            fds.as_ptr(),
            libc::CMSG_DATA(cmsg).cast::<RawFd>(),
            fds.len(),
        );
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

fn recv_first_with_fds(sock: RawFd) -> Result<(u8, Vec<OwnedFd>), FrameReceiveError> {
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
    message.msg_controllen = control.len() as _;
    // SAFETY: every pointer in `message` references writable live storage for recvmsg.
    let received = unsafe { libc::recvmsg(sock, &mut message, libc::MSG_CMSG_CLOEXEC) };
    if received < 0 {
        return Err(FrameReceiveError::Transport(io::Error::last_os_error()));
    }
    if received == 0 {
        return Err(FrameReceiveError::Transport(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "input IPC peer closed",
        )));
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
    if message.msg_flags & libc::MSG_CTRUNC != 0 {
        drop(fds);
        return Err(FrameReceiveError::ControlTruncated);
    }
    Ok((first, fds))
}
