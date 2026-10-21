use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;

use pf_input_broker::session_ipc::{
    authorize_peer, bind_endpoint, read_frame_with_fd, read_registered_frame_with,
    serve_compositor_peer, serve_keyboard_peer, serve_menu_peer, write_frame_with_fd,
    write_frame_with_fds, validate_role_uid_mapping, CompositorAuthority,
    EndpointAdmissionError, EndpointPolicy, PeerFacts, RegisteredFrameError, RoleUidMappingError,
};
use pf_input_ipc::compositor::{
    CompositorSnapshot, CompositorToRouter, FocusSnapshot, FocusedSurface, OverlaySnapshot,
    OverlayState, ProtectedPowerSnapshot, RouterToCompositor, TextInputState,
};
use pf_input_ipc::Role;
use pf_input_ipc::{ContentPurpose, OverlayRole};

#[test]
fn endpoint_admission_requires_uid_role_group_and_exact_unit() {
    let policy = EndpointPolicy {
        role: Role::Keyboard,
        expected_uid: Some(1000),
        role_gid: 2001,
        unit: "pf-osk.service",
    };
    let good = PeerFacts {
        uid: 1000,
        groups: vec![2001, 3000],
        unit: "pf-osk.service".into(),
    };
    assert_eq!(authorize_peer(&policy, &good).unwrap(), Role::Keyboard);

    assert_eq!(
        authorize_peer(
            &policy,
            &PeerFacts {
                uid: 1001,
                ..good.clone()
            }
        ),
        Err(EndpointAdmissionError::UidMismatch {
            role: Role::Keyboard,
            got: 1001,
            expected: 1000,
        })
    );
    assert_eq!(
        authorize_peer(
            &policy,
            &PeerFacts {
                groups: vec![3000],
                ..good.clone()
            }
        ),
        Err(EndpointAdmissionError::GroupMismatch {
            role: Role::Keyboard,
            expected_gid: 2001,
        })
    );
    assert_eq!(
        authorize_peer(
            &policy,
            &PeerFacts {
                unit: "pf-app@impostor.service".into(),
                ..good
            }
        ),
        Err(EndpointAdmissionError::UnitMismatch {
            role: Role::Keyboard,
            got: "pf-app@impostor.service".into(),
            expected: "pf-osk.service",
        })
    );
}

#[test]
fn role_uid_mapping_rejects_aliases_at_startup() {
    assert_eq!(
        validate_role_uid_mapping(&[
            (Role::Compositor, 1001),
            (Role::Keyboard, 1002),
            (Role::Menu, 1003),
        ]),
        Ok(())
    );
    assert_eq!(
        validate_role_uid_mapping(&[
            (Role::Compositor, 1001),
            (Role::Keyboard, 1001),
        ]),
        Err(RoleUidMappingError::DuplicateUid {
            uid: 1001,
            first: Role::Compositor,
            second: Role::Keyboard,
        })
    );
}

#[test]
fn missing_menu_unit_policy_refuses_every_registration() {
    let unavailable = EndpointPolicy {
        role: Role::Menu,
        expected_uid: None,
        role_gid: 2002,
        unit: "pf-system-menu.service",
    };
    let otherwise_valid = PeerFacts {
        uid: 1003,
        groups: vec![2002],
        unit: "pf-system-menu.service".into(),
    };
    assert_eq!(
        authorize_peer(&unavailable, &otherwise_valid),
        Err(EndpointAdmissionError::RoleUnavailable { role: Role::Menu })
    );
}

#[test]
fn framed_descriptor_transfer_is_bounded_and_preserves_first_byte_ancillary_data() {
    let (mut sender, mut receiver) = UnixStream::pair().unwrap();
    let source = std::fs::File::open("/dev/null").unwrap();
    let body = br#"{"version":1,"type":"registered"}"#;

    write_frame_with_fd(&mut sender, body, source.as_raw_fd()).unwrap();
    let (received, fd) = read_frame_with_fd(&mut receiver).unwrap();
    assert_eq!(received, body);
    let mut fd = std::fs::File::from(fd.expect("registered carries one fd"));
    let mut byte = [0_u8; 1];
    assert_eq!(
        fd.read(&mut byte).unwrap(),
        0,
        "received fd names /dev/null"
    );

    let oversized = vec![0_u8; pf_input_ipc::MAX_FRAME + 1];
    let error = write_frame_with_fd(&mut sender, &oversized, source.as_raw_fd()).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn registered_frame_has_ordered_pad_and_wayland_fds_and_closes_both_on_exec() {
    let (mut sender, mut receiver) = UnixStream::pair().unwrap();
    let pad = std::fs::File::open("/dev/null").unwrap();
    let (wayland, mut wayland_peer) = UnixStream::pair().unwrap();
    let body = pf_input_ipc::encode(&pf_input_ipc::menu::RouterToMenu::Registered);

    write_frame_with_fds(&mut sender, &body, &[pad.as_raw_fd(), wayland.as_raw_fd()]).unwrap();
    let (received, descriptors) = read_registered_frame_with(&mut receiver, |_| Ok(())).unwrap();
    assert_eq!(received, body);
    for fd in [&descriptors.pad, &descriptors.wayland] {
        assert_ne!(
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0,
            "every received descriptor is close-on-exec"
        );
    }
    let mut trusted_display = UnixStream::from(descriptors.wayland);
    wayland_peer.write_all(b"w").unwrap();
    let mut byte = [0_u8; 1];
    trusted_display.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [b'w'], "the second descriptor is the Wayland socket");
}

#[test]
fn registered_frame_refuses_count_order_and_truncation_without_leaking_fds() {
    assert_registered_rejected(1, RegisteredFrameError::DescriptorCount { got: 1 }, |_| {
        Ok(())
    });
    assert_registered_rejected(3, RegisteredFrameError::DescriptorCount { got: 3 }, |_| {
        Ok(())
    });
    assert_registered_rejected(2, RegisteredFrameError::FirstNotEvdev, |_| {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "not evdev",
        ))
    });
    assert_registered_rejected(9, RegisteredFrameError::ControlTruncated, |_| Ok(()));
}

fn assert_registered_rejected(
    count: usize,
    expected: RegisteredFrameError,
    validate_pad: impl FnOnce(std::os::fd::BorrowedFd<'_>) -> std::io::Result<()>,
) {
    let (mut sender, mut receiver) = UnixStream::pair().unwrap();
    let mut peers = Vec::new();
    let mut sent = Vec::new();
    for _ in 0..count {
        let (peer, descriptor) = UnixStream::pair().unwrap();
        peers.push(peer);
        sent.push(descriptor);
    }
    let body = pf_input_ipc::encode(&pf_input_ipc::menu::RouterToMenu::Registered);
    let raw: Vec<_> = sent.iter().map(AsRawFd::as_raw_fd).collect();
    write_frame_with_fds(&mut sender, &body, &raw).unwrap();
    drop(sent);
    let error = read_registered_frame_with(&mut receiver, validate_pad).unwrap_err();
    assert_eq!(error, expected);
    for mut peer in peers {
        peer.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let mut byte = [0_u8; 1];
        assert_eq!(
            peer.read(&mut byte).unwrap(),
            0,
            "rejected registration must close every received descriptor"
        );
    }
}

#[test]
fn missing_descriptor_is_a_negative_not_a_partial_read() {
    let (mut sender, mut receiver) = UnixStream::pair().unwrap();
    let body = br#"{"version":1,"type":"registered"}"#;
    sender
        .write_all(&(body.len() as u32).to_be_bytes())
        .unwrap();
    sender.write_all(body).unwrap();

    let (received, fd) = read_frame_with_fd(&mut receiver).unwrap();
    assert_eq!(received, body);
    assert!(fd.is_none());
}

#[test]
fn compositor_must_register_then_replay_snapshot_for_its_new_epoch() {
    let authority = std::sync::Arc::new(CompositorAuthority::default());
    let (mut client, server) = UnixStream::pair().unwrap();
    let serving = authority.clone();
    let thread = std::thread::spawn(move || serve_compositor_peer(server, &serving));

    send(&mut client, &CompositorToRouter::Register);
    let (body, fd) = read_frame_with_fd(&mut client).unwrap();
    assert!(
        fd.is_none(),
        "compositor registration never receives a pad fd"
    );
    let RouterToCompositor::Registered { epoch } = pf_input_ipc::decode(&body).unwrap() else {
        panic!("first reply must be registered");
    };
    assert_ne!(epoch, 0);

    send(
        &mut client,
        &CompositorToRouter::StateSnapshot {
            snapshot: snapshot(epoch),
        },
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while !authority.snapshot_ready(epoch) && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(authority.snapshot_ready(epoch));
    assert!(!authority.secure_entry(), "secure_entry is relayed exactly");

    drop(client);
    thread.join().unwrap().unwrap();
    assert!(authority.secure_entry(), "disconnect fails closed");
}

#[test]
fn superseded_compositor_disconnect_cannot_clear_the_new_epoch() {
    let authority = std::sync::Arc::new(CompositorAuthority::default());
    let (mut old_client, old_server) = UnixStream::pair().unwrap();
    let old_authority = authority.clone();
    let old_thread = std::thread::spawn(move || serve_compositor_peer(old_server, &old_authority));
    send(&mut old_client, &CompositorToRouter::Register);
    let (old_body, _) = read_frame_with_fd(&mut old_client).unwrap();
    let RouterToCompositor::Registered { epoch: old_epoch } =
        pf_input_ipc::decode(&old_body).unwrap()
    else {
        panic!("old compositor was not registered");
    };

    let (mut new_client, new_server) = UnixStream::pair().unwrap();
    let new_authority = authority.clone();
    let new_thread = std::thread::spawn(move || serve_compositor_peer(new_server, &new_authority));
    send(&mut new_client, &CompositorToRouter::Register);
    let (new_body, _) = read_frame_with_fd(&mut new_client).unwrap();
    let RouterToCompositor::Registered { epoch: new_epoch } =
        pf_input_ipc::decode(&new_body).unwrap()
    else {
        panic!("new compositor was not registered");
    };
    assert!(new_epoch > old_epoch);
    send(
        &mut new_client,
        &CompositorToRouter::StateSnapshot {
            snapshot: snapshot(new_epoch),
        },
    );
    wait_for_snapshot(&authority, new_epoch);

    drop(old_client);
    old_thread.join().unwrap().unwrap();
    assert!(
        authority.snapshot_ready(new_epoch),
        "old EOF must not disconnect the replacement"
    );
    assert!(!authority.secure_entry());

    drop(new_client);
    new_thread.join().unwrap().unwrap();
    assert!(authority.secure_entry());
}

#[test]
fn trusted_fd_is_never_delivered_across_an_epoch_or_before_snapshot() {
    let authority = std::sync::Arc::new(CompositorAuthority::default());
    let (mut old_client, old_server) = UnixStream::pair().unwrap();
    let old_authority = authority.clone();
    let old_thread = std::thread::spawn(move || serve_compositor_peer(old_server, &old_authority));
    send(&mut old_client, &CompositorToRouter::Register);
    let (old_body, _) = read_frame_with_fd(&mut old_client).unwrap();
    let RouterToCompositor::Registered { epoch: old_epoch } =
        pf_input_ipc::decode(&old_body).unwrap()
    else {
        panic!("old compositor was not registered");
    };
    send(
        &mut old_client,
        &CompositorToRouter::StateSnapshot {
            snapshot: snapshot(old_epoch),
        },
    );
    wait_for_snapshot(&authority, old_epoch);

    let stale = authority
        .request_trusted_connection(OverlayRole::Menu)
        .unwrap();
    let (request_body, request_fd) = read_frame_with_fd(&mut old_client).unwrap();
    assert!(request_fd.is_none());
    let RouterToCompositor::TrustedConnection {
        request_id: stale_id,
        ..
    } = pf_input_ipc::decode(&request_body).unwrap()
    else {
        panic!("trusted connection was not requested");
    };

    let (mut new_client, new_server) = UnixStream::pair().unwrap();
    let new_authority = authority.clone();
    let new_thread = std::thread::spawn(move || serve_compositor_peer(new_server, &new_authority));
    send(&mut new_client, &CompositorToRouter::Register);
    let (new_body, _) = read_frame_with_fd(&mut new_client).unwrap();
    let RouterToCompositor::Registered { epoch: new_epoch } =
        pf_input_ipc::decode(&new_body).unwrap()
    else {
        panic!("new compositor was not registered");
    };

    assert!(
        authority
            .request_trusted_connection(OverlayRole::Menu)
            .is_err(),
        "no trusted fd may be requested before snapshot replay"
    );
    send_with_fd(
        &mut old_client,
        &CompositorToRouter::TrustedConnectionResult {
            epoch: old_epoch,
            request_id: stale_id,
            result: pf_input_ipc::compositor::TrustedConnectionOutcome::Ok,
        },
    );
    assert!(
        stale
            .recv_timeout(std::time::Duration::from_secs(1))
            .is_err(),
        "re-registration invalidates the old epoch's pending delivery"
    );

    send(
        &mut new_client,
        &CompositorToRouter::StateSnapshot {
            snapshot: snapshot(new_epoch),
        },
    );
    wait_for_snapshot(&authority, new_epoch);
    let fresh = authority
        .request_trusted_connection(OverlayRole::Menu)
        .unwrap();
    let (request_body, request_fd) = read_frame_with_fd(&mut new_client).unwrap();
    assert!(request_fd.is_none());
    let RouterToCompositor::TrustedConnection {
        request_id: fresh_id,
        ..
    } = pf_input_ipc::decode(&request_body).unwrap()
    else {
        panic!("fresh trusted connection was not requested");
    };
    send_with_fd(
        &mut new_client,
        &CompositorToRouter::TrustedConnectionResult {
            epoch: new_epoch,
            request_id: fresh_id,
            result: pf_input_ipc::compositor::TrustedConnectionOutcome::Ok,
        },
    );
    let received: OwnedFd = fresh
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap()
        .unwrap();
    assert_eq!(
        unsafe { libc::fcntl(received.as_raw_fd(), libc::F_GETFD) } >= 0,
        true
    );

    drop(old_client);
    drop(new_client);
    assert!(old_thread.join().unwrap().is_err());
    new_thread.join().unwrap().unwrap();
}

#[test]
fn menu_registration_receives_pad_then_current_epoch_wayland_fd() {
    let authority = std::sync::Arc::new(CompositorAuthority::default());
    let (mut compositor, compositor_server) = UnixStream::pair().unwrap();
    let compositor_authority = authority.clone();
    let compositor_thread =
        std::thread::spawn(move || serve_compositor_peer(compositor_server, &compositor_authority));
    send(&mut compositor, &CompositorToRouter::Register);
    let (registered, _) = read_frame_with_fd(&mut compositor).unwrap();
    let RouterToCompositor::Registered { epoch } = pf_input_ipc::decode(&registered).unwrap()
    else {
        panic!("compositor was not registered");
    };
    send(
        &mut compositor,
        &CompositorToRouter::StateSnapshot {
            snapshot: snapshot(epoch),
        },
    );
    wait_for_snapshot(&authority, epoch);

    let (mut menu, menu_server) = UnixStream::pair().unwrap();
    let menu_authority = authority.clone();
    let pad = std::fs::File::open("/dev/null").unwrap();
    let menu_thread =
        std::thread::spawn(move || serve_menu_peer(menu_server, &menu_authority, pad.into()));
    let register = pf_input_ipc::encode(&pf_input_ipc::menu::MenuToRouter::Register);
    pf_wire::write_frame(&mut menu, &register).unwrap();

    let (request, request_fd) = read_frame_with_fd(&mut compositor).unwrap();
    assert!(request_fd.is_none());
    let RouterToCompositor::TrustedConnection {
        epoch: request_epoch,
        request_id,
        role: OverlayRole::Menu,
        unit,
    } = pf_input_ipc::decode(&request).unwrap()
    else {
        panic!("menu trust was not requested");
    };
    assert_eq!(request_epoch, epoch);
    assert_eq!(unit, "pf-system-menu.service");
    let (wayland, mut gamescope_wayland) = UnixStream::pair().unwrap();
    let response = CompositorToRouter::TrustedConnectionResult {
        epoch,
        request_id,
        result: pf_input_ipc::compositor::TrustedConnectionOutcome::Ok,
    };
    let response = pf_input_ipc::encode(&response);
    write_frame_with_fd(&mut compositor, &response, wayland.as_raw_fd()).unwrap();

    let (registered, descriptors) = read_registered_frame_with(&mut menu, |_| Ok(())).unwrap();
    assert_eq!(
        pf_input_ipc::decode::<pf_input_ipc::menu::RouterToMenu>(&registered).unwrap(),
        pf_input_ipc::menu::RouterToMenu::Registered
    );
    let mut pad = std::fs::File::from(descriptors.pad);
    let mut byte = [0_u8; 1];
    assert_eq!(pad.read(&mut byte).unwrap(), 0, "pad fd is first");
    let mut trusted_wayland = UnixStream::from(descriptors.wayland);
    gamescope_wayland.write_all(b"t").unwrap();
    trusted_wayland.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [b't'], "trusted Wayland fd is second");

    drop(menu);
    menu_thread.join().unwrap().unwrap();
    drop(compositor);
    compositor_thread.join().unwrap().unwrap();
}

#[test]
fn keyboard_registration_receives_pad_then_current_epoch_wayland_fd() {
    let authority = std::sync::Arc::new(CompositorAuthority::default());
    let (mut compositor, compositor_server) = UnixStream::pair().unwrap();
    let compositor_authority = authority.clone();
    let compositor_thread =
        std::thread::spawn(move || serve_compositor_peer(compositor_server, &compositor_authority));
    send(&mut compositor, &CompositorToRouter::Register);
    let (registered, _) = read_frame_with_fd(&mut compositor).unwrap();
    let RouterToCompositor::Registered { epoch } = pf_input_ipc::decode(&registered).unwrap()
    else {
        panic!("compositor was not registered");
    };
    send(
        &mut compositor,
        &CompositorToRouter::StateSnapshot {
            snapshot: snapshot(epoch),
        },
    );
    wait_for_snapshot(&authority, epoch);

    let (mut keyboard, keyboard_server) = UnixStream::pair().unwrap();
    let keyboard_authority = authority.clone();
    let pad = std::fs::File::open("/dev/null").unwrap();
    let keyboard_thread = std::thread::spawn(move || {
        serve_keyboard_peer(keyboard_server, &keyboard_authority, pad.into())
    });
    let register = pf_input_ipc::encode(&pf_input_ipc::keyboard::KeyboardToRouter::Register {
        surfaces: vec![pf_input_ipc::keyboard::KeyboardSurface::Docked],
    });
    pf_wire::write_frame(&mut keyboard, &register).unwrap();

    let (request, request_fd) = read_frame_with_fd(&mut compositor).unwrap();
    assert!(request_fd.is_none());
    let RouterToCompositor::TrustedConnection {
        epoch: request_epoch,
        request_id,
        role: OverlayRole::Keyboard,
        unit,
    } = pf_input_ipc::decode(&request).unwrap()
    else {
        panic!("keyboard trust was not requested");
    };
    assert_eq!(request_epoch, epoch);
    assert_eq!(unit, "pf-osk.service");
    let (wayland, mut gamescope_wayland) = UnixStream::pair().unwrap();
    let response = CompositorToRouter::TrustedConnectionResult {
        epoch,
        request_id,
        result: pf_input_ipc::compositor::TrustedConnectionOutcome::Ok,
    };
    write_frame_with_fd(
        &mut compositor,
        &pf_input_ipc::encode(&response),
        wayland.as_raw_fd(),
    )
    .unwrap();

    let (registered, descriptors) = read_registered_frame_with(&mut keyboard, |_| Ok(())).unwrap();
    assert_eq!(
        pf_input_ipc::decode::<pf_input_ipc::keyboard::RouterToKeyboard>(&registered).unwrap(),
        pf_input_ipc::keyboard::RouterToKeyboard::Registered
    );
    let mut pad = std::fs::File::from(descriptors.pad);
    let mut byte = [0_u8; 1];
    assert_eq!(pad.read(&mut byte).unwrap(), 0, "pad fd is first");
    let mut trusted_wayland = UnixStream::from(descriptors.wayland);
    gamescope_wayland.write_all(b"t").unwrap();
    trusted_wayland.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [b't'], "trusted Wayland fd is second");

    drop(keyboard);
    keyboard_thread.join().unwrap().unwrap();
    drop(compositor);
    compositor_thread.join().unwrap().unwrap();
}

fn send(stream: &mut UnixStream, message: &CompositorToRouter) {
    let body = pf_input_ipc::encode(message);
    pf_wire::write_frame(stream, &body).unwrap();
}

fn send_with_fd(stream: &mut UnixStream, message: &CompositorToRouter) {
    let body = pf_input_ipc::encode(message);
    let source = std::fs::File::open("/dev/null").unwrap();
    write_frame_with_fd(stream, &body, source.as_raw_fd()).unwrap();
}

fn wait_for_snapshot(authority: &CompositorAuthority, epoch: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while !authority.snapshot_ready(epoch) && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(authority.snapshot_ready(epoch));
}

fn snapshot(epoch: u64) -> CompositorSnapshot {
    CompositorSnapshot {
        epoch,
        focus: FocusSnapshot {
            surface: FocusedSurface::None,
            text_input: TextInputState {
                active: false,
                purpose: ContentPurpose::Normal,
            },
            secure_entry: false,
        },
        overlays: vec![
            overlay(OverlayRole::Keyboard),
            overlay(OverlayRole::Menu),
            overlay(OverlayRole::Toast),
        ],
        power: ProtectedPowerSnapshot {
            pressed: false,
            last_sequence: 0,
        },
    }
}

fn overlay(role: OverlayRole) -> OverlaySnapshot {
    OverlaySnapshot {
        role,
        state: OverlayState::Unmapped,
        rect: None,
    }
}

#[test]
fn endpoint_binding_sets_contract_mode_and_refuses_non_socket_collisions() {
    let root = std::env::temp_dir().join(format!("pfi-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let socket = root.join("compositor.sock");
    let policy = EndpointPolicy {
        role: Role::Compositor,
        expected_uid: Some(unsafe { libc::geteuid() }),
        role_gid: unsafe { libc::getegid() },
        unit: "pocketforge-gamescope.service",
    };

    let listener = bind_endpoint(&socket, &policy).unwrap();
    assert_eq!(
        std::fs::symlink_metadata(&socket)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        pf_input_ipc::sockets::SOCKET_MODE
    );
    drop(listener);
    std::fs::remove_file(&socket).unwrap();

    std::fs::write(&socket, b"do not replace").unwrap();
    let error = bind_endpoint(&socket, &policy).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&socket).unwrap(), b"do not replace");

    std::fs::remove_file(socket).unwrap();
    std::fs::remove_dir(root).unwrap();
}
