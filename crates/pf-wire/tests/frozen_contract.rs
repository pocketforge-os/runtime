//! PFW1 v1 FROZEN CONTRACT (`tsp-e1b.5`). These assertions pin the wire surface that
//! `libpocketforge` and any reimplementation depend on. A change here is a DELIBERATE,
//! version-bumping act — never silent. If you renumber an enum, change `WIRE_VERSION`, alter the
//! pose layout, or change the byte encoding of a canonical message, THIS TEST FAILS — that is the
//! point (see `STABILITY.md`).

use pf_wire::{
    recv_request, recv_response, send_request, send_response, Op, Permission, Request, Response,
    RumbleStatus, Status, MAX_FRAME, WIRE_VERSION,
};

#[test]
fn wire_version_and_frame_bound_are_frozen() {
    assert_eq!(WIRE_VERSION, 1, "WIRE_VERSION is part of the frozen contract");
    assert_eq!(MAX_FRAME, 64 * 1024, "MAX_FRAME (DoS bound) is frozen");
}

#[test]
fn op_discriminants_are_frozen() {
    assert_eq!(Op::IsPresent as u8, 1);
    assert_eq!(Op::IsGranted as u8, 2);
    assert_eq!(Op::Query as u8, 3);
    assert_eq!(Op::Acquire as u8, 4);
    assert_eq!(Op::GetCapability as u8, 5);
    assert_eq!(Op::SetCapability as u8, 6);
    assert_eq!(Op::RumblePulse as u8, 7);
    assert_eq!(Op::GetPose as u8, 8);
    assert_eq!(Op::SetPose as u8, 9);
}

#[test]
fn status_permission_rumble_discriminants_are_frozen() {
    // Status (the four-way taxonomy; mirrors the C ABI PF_* codes).
    assert_eq!(Status::Ok as u8, 0);
    assert_eq!(Status::Unsupported as u8, 1);
    assert_eq!(Status::PolicyBlocked as u8, 2);
    assert_eq!(Status::ConsentDenied as u8, 3);
    assert_eq!(Status::HardwareAbsent as u8, 4);
    // Permission (Permissions-API query() shape).
    assert_eq!(Permission::Granted as u8, 0);
    assert_eq!(Permission::Denied as u8, 1);
    assert_eq!(Permission::Prompt as u8, 2);
    // RumbleStatus (the unified cosmetic no-op shape).
    assert_eq!(RumbleStatus::Fired as u8, 0);
    assert_eq!(RumbleStatus::NoopAbsent as u8, 1);
    assert_eq!(RumbleStatus::NoopSuppressed as u8, 2);
}

/// Encode a value via its framed writer to a `Vec<u8>` (len-prefix + body).
fn enc_req(r: &Request) -> Vec<u8> {
    let mut v = Vec::new();
    send_request(&mut v, r).unwrap();
    v
}
fn enc_resp(r: &Response) -> Vec<u8> {
    let mut v = Vec::new();
    send_response(&mut v, r).unwrap();
    v
}

#[test]
fn canonical_message_encodings_are_frozen() {
    // A canonical Acquire("imu") request and an Ok+flag=1 response. The exact bytes are the
    // contract a second-language client must reproduce (golden, captured from the impl).
    let mut req = Request::new(Op::Acquire, "imu");
    req.arg = 0;
    let mut resp = Response::ok();
    resp.flag = 1;

    let req_bytes = enc_req(&req);
    let resp_bytes = enc_resp(&resp);

    // GOLDEN (PFW1 v1), captured from the implementation: big-endian u32 length prefix +
    // protobuf-wire body. Request body = field1(op,varint)=4 Acquire, field2(name,len)="imu".
    // Response body = field1(status,varint)=0 Ok, field3(flag,varint)=1.
    assert_eq!(
        hex(&req_bytes),
        "0000000708041203696d75",
        "frozen Acquire(\"imu\") request encoding"
    );
    assert_eq!(hex(&resp_bytes), "0000000408001801", "frozen Ok+flag=1 response encoding");

    // …and they round-trip through the readers (decode is part of the contract too).
    let mut c = std::io::Cursor::new(&req_bytes);
    let back = recv_request(&mut c).unwrap();
    assert_eq!(back.op, Op::Acquire);
    assert_eq!(back.name, "imu");
    let mut c = std::io::Cursor::new(&resp_bytes);
    let rback = recv_response(&mut c).unwrap();
    assert_eq!(rback.status, Status::Ok);
    assert_eq!(rback.flag, 1);
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
