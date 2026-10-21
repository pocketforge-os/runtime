use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use pf_input_broker::session_ipc::{
    authorize_peer, read_frame_with_fd, write_frame_with_fd, EndpointPolicy, PeerFacts,
};
use pf_input_ipc::Role;

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
        PeerFacts { uid: 1001, ..good.clone() },
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
    assert_eq!(fd.read(&mut byte).unwrap(), 0, "received fd names /dev/null");

    let oversized = vec![0_u8; pf_input_ipc::MAX_FRAME + 1];
    let error = write_frame_with_fd(&mut sender, &oversized, source.as_raw_fd()).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn missing_descriptor_is_a_negative_not_a_partial_read() {
    let (mut sender, mut receiver) = UnixStream::pair().unwrap();
    let body = br#"{"version":1,"type":"registered"}"#;
    sender.write_all(&(body.len() as u32).to_be_bytes()).unwrap();
    sender.write_all(body).unwrap();

    let (received, fd) = read_frame_with_fd(&mut receiver).unwrap();
    assert_eq!(received, body);
    assert!(fd.is_none());
}
