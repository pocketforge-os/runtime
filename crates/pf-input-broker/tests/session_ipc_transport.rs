use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;

use pf_input_broker::session_ipc::{
    authorize_peer, bind_endpoint, read_frame_with_fd, serve_compositor_peer, write_frame_with_fd,
    CompositorAuthority, EndpointPolicy, PeerFacts,
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
        session_uid: 1000,
        role_gid: 2001,
        unit: "pf-osk.service",
    };
    let good = PeerFacts {
        uid: 1000,
        groups: vec![2001, 3000],
        unit: "pf-osk.service".into(),
    };
    assert_eq!(authorize_peer(&policy, &good).unwrap(), Role::Keyboard);

    for bad in [
        PeerFacts {
            uid: 1001,
            ..good.clone()
        },
        PeerFacts {
            groups: vec![3000],
            ..good.clone()
        },
        PeerFacts {
            unit: "pf-app@impostor.service".into(),
            ..good
        },
    ] {
        let error = authorize_peer(&policy, &bad).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }
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

fn send(stream: &mut UnixStream, message: &CompositorToRouter) {
    let body = pf_input_ipc::encode(message);
    pf_wire::write_frame(stream, &body).unwrap();
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
        session_uid: unsafe { libc::geteuid() },
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
