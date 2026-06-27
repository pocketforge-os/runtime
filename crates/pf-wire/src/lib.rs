//! # PFW1 — the PocketForge capability-broker wire protocol
//!
//! A **documented, versioned, reimplementable** contract (epic `tsp-e1b` / child `.2`).
//! The same wire is spoken by `libpocketforge`'s out-of-process [`broker_client`] backend
//! and served by the reference broker (`pf-broker-ref`) and the real broker daemon
//! (`.3`). The full byte-level spec lives in [`wire/WIRE-PROTOCOL.md`](../../wire/WIRE-PROTOCOL.md);
//! this module is one conforming implementation, not the source of truth.
//!
//! ## Two channels (folds in SPIKE-1, `tsp-e1b.1`)
//!
//! SPIKE-1 measured a broker round-trip at ~25× the cost of a shared-fd `read()` and,
//! crucially, found it couples the broker's ms-scale scheduling tail to the caller. The
//! verdict the wire encodes:
//!
//! * **Control channel (this protocol).** Request/reply RPC for every *low-rate* capability
//!   (vibration, sensors, location, audio, settings, entropy, presence/permission queries).
//!   At an A53-estimated ~100 µs p99 per round-trip these are <0.001% of a 60 Hz frame; any
//!   capability under ~1000 calls/sec is comfortably fine.
//! * **INPUT is NOT on this channel.** The input hot path is delivered as a *handed/shared
//!   file descriptor* (the `uinput`+`EVIOCGRAB` device, child `.6`) the app reads directly —
//!   never call-per-sample RPC. PFW1 only carries the input capability's *acquisition* (a
//!   request that yields the fd via `SCM_RIGHTS`, specified in `.6`), not its per-event stream.
//!
//! ## Framing
//!
//! Each message is `len: u32` (big-endian) followed by exactly `len` bytes of body. `len`
//! must not exceed [`MAX_FRAME`]; an over-long prefix is a protocol error (DoS bound).
//!
//! ## Body encoding
//!
//! The body uses **protobuf wire format** primitives so it is trivially reimplementable:
//! a sequence of `(key, value)` fields where `key = (field_number << 3) | wire_type`,
//! encoded as a base-128 varint. Only two wire types are used:
//! `0` = varint (ints / enums) and `2` = length-delimited (a varint byte-count then the
//! bytes; used for UTF-8 names and opaque payloads). Unknown fields are skipped, so the
//! protocol can grow without breaking old peers.

use std::io::{self, Read, Write};

/// Wire format identifier, surfaced in the spec + handshake docs.
pub const WIRE_VERSION: u32 = 1;

/// Maximum body length accepted from a peer (64 KiB). Bounds a hostile/buggy length prefix.
pub const MAX_FRAME: usize = 64 * 1024;

// --- protobuf wire types we use ---
const WT_VARINT: u64 = 0;
const WT_LEN: u64 = 2;

/// Errors from encoding, decoding, or framing a PFW1 message.
#[derive(Debug)]
pub enum WireError {
    /// An I/O error reading/writing the underlying stream.
    Io(io::Error),
    /// The frame's declared length exceeds [`MAX_FRAME`].
    FrameTooLarge(usize),
    /// A varint ran past 10 bytes / past the buffer, or a length-delimited field overran.
    Truncated,
    /// A field carried a wire type the decoder does not expect for it.
    BadWireType { field: u64, got: u64 },
    /// An enum varint did not map to a known value.
    BadEnum { field: &'static str, value: u64 },
    /// A length-delimited field that must be UTF-8 was not.
    BadUtf8,
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Io(e) => write!(f, "io: {e}"),
            WireError::FrameTooLarge(n) => write!(f, "frame too large: {n} > {MAX_FRAME}"),
            WireError::Truncated => write!(f, "truncated message"),
            WireError::BadWireType { field, got } => {
                write!(f, "field {field}: unexpected wire type {got}")
            }
            WireError::BadEnum { field, value } => write!(f, "field {field}: bad enum {value}"),
            WireError::BadUtf8 => write!(f, "non-utf8 string field"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<io::Error> for WireError {
    fn from(e: io::Error) -> Self {
        WireError::Io(e)
    }
}

type Result<T> = std::result::Result<T, WireError>;

// ---------------------------------------------------------------------------
// Enums carried on the wire (stable numeric values — part of the contract).
// ---------------------------------------------------------------------------

/// Operation a [`Request`] asks the broker to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Op {
    /// Side-effect-free presence check (descriptor + live probe). -> `flag` = 0/1.
    IsPresent = 1,
    /// Side-effect-free grant check (present AND policy-allowed). -> `flag` = 0/1.
    IsGranted = 2,
    /// Permissions-API `query()`: -> `permission` (Granted/Denied/Prompt).
    Query = 3,
    /// Acquire a capability handle. -> `status` (four-way taxonomy) or Ok.
    Acquire = 4,
    /// Cooperative get of a capability's last value. -> `status` + `payload`.
    GetCapability = 5,
    /// Cooperative set of a capability's value (`payload`). -> `status`.
    SetCapability = 6,
    /// Pulse the rumble actuator for `arg` ms. -> `flag` = [`RumbleStatus`].
    RumblePulse = 7,
    /// Read the current IMU pose. -> `status` + `payload` (9× f64 LE, 72 bytes).
    GetPose = 8,
    /// Set the IMU pose absolutely (`payload` = 9× f64 LE). -> `status` + `payload` (new pose).
    SetPose = 9,
}

impl Op {
    fn from_u64(v: u64) -> Result<Op> {
        Ok(match v {
            1 => Op::IsPresent,
            2 => Op::IsGranted,
            3 => Op::Query,
            4 => Op::Acquire,
            5 => Op::GetCapability,
            6 => Op::SetCapability,
            7 => Op::RumblePulse,
            8 => Op::GetPose,
            9 => Op::SetPose,
            _ => return Err(WireError::BadEnum { field: "op", value: v }),
        })
    }
}

/// The four-way typed-error taxonomy (briefing §A) carried in a [`Response`].
/// `Ok` is the success case; the other four are the taxonomy. This is the SAME
/// shape `pocketforge::CapError` exposes to Rust callers — the wire mirrors the type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    Ok = 0,
    /// The platform has no such capability type at all.
    Unsupported = 1,
    /// Refused by policy (e.g. default-deny on a privacy cap).
    PolicyBlocked = 2,
    /// The user/consent layer denied it.
    ConsentDenied = 3,
    /// The descriptor advertises no such hardware on this device.
    HardwareAbsent = 4,
}

impl Status {
    fn from_u64(v: u64) -> Result<Status> {
        Ok(match v {
            0 => Status::Ok,
            1 => Status::Unsupported,
            2 => Status::PolicyBlocked,
            3 => Status::ConsentDenied,
            4 => Status::HardwareAbsent,
            _ => return Err(WireError::BadEnum { field: "status", value: v }),
        })
    }
}

/// The side-effect-free permission state from `query()` (Permissions-API shape).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Permission {
    Granted = 0,
    Denied = 1,
    Prompt = 2,
}

impl Permission {
    fn from_u64(v: u64) -> Result<Permission> {
        Ok(match v {
            0 => Permission::Granted,
            1 => Permission::Denied,
            2 => Permission::Prompt,
            _ => return Err(WireError::BadEnum { field: "permission", value: v }),
        })
    }
}

/// Outcome of a rumble pulse — the unified no-op shape (mirrors the sim's `RumbleHandle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RumbleStatus {
    /// Motor present AND haptics enabled -> would actuate.
    Fired = 0,
    /// Descriptor advertises no rumble motor (e.g. a133).
    NoopAbsent = 1,
    /// Motor present but the `hapticsEnabled` preference is off (E4).
    NoopSuppressed = 2,
}

impl RumbleStatus {
    /// Map a raw wire flag to a status (clamping unknowns to `Fired` would lie; error instead).
    pub fn from_u64(v: u64) -> Result<RumbleStatus> {
        Ok(match v {
            0 => RumbleStatus::Fired,
            1 => RumbleStatus::NoopAbsent,
            2 => RumbleStatus::NoopSuppressed,
            _ => return Err(WireError::BadEnum { field: "rumble_status", value: v }),
        })
    }
}

// ---------------------------------------------------------------------------
// Messages.
// ---------------------------------------------------------------------------

/// A request from `libpocketforge` to the broker.
///
/// Field numbers (see the spec): 1=op (varint), 2=name (len-delim utf8),
/// 3=payload (len-delim bytes), 4=arg (varint).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub op: Op,
    /// Capability name, e.g. `"vibration"`, `"location"`, `"imu"`.
    pub name: String,
    /// Optional opaque payload (e.g. a value to `SetCapability`).
    pub payload: Vec<u8>,
    /// Optional scalar argument (e.g. rumble duration in ms).
    pub arg: u64,
}

impl Request {
    /// Construct a request carrying only an op + capability name.
    pub fn new(op: Op, name: impl Into<String>) -> Request {
        Request { op, name: name.into(), payload: Vec::new(), arg: 0 }
    }

    /// Encode to the PFW1 body (no length prefix; see [`write_frame`]).
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(16 + self.name.len() + self.payload.len());
        put_varint_field(&mut b, 1, self.op as u64);
        if !self.name.is_empty() {
            put_len_field(&mut b, 2, self.name.as_bytes());
        }
        if !self.payload.is_empty() {
            put_len_field(&mut b, 3, &self.payload);
        }
        if self.arg != 0 {
            put_varint_field(&mut b, 4, self.arg);
        }
        b
    }

    /// Decode a PFW1 body into a request.
    pub fn decode(buf: &[u8]) -> Result<Request> {
        let mut op: Option<Op> = None;
        let mut name = String::new();
        let mut payload = Vec::new();
        let mut arg = 0u64;
        for field in FieldIter::new(buf) {
            let (num, val) = field?;
            match (num, val) {
                (1, FieldVal::Varint(v)) => op = Some(Op::from_u64(v)?),
                (2, FieldVal::Len(b)) => {
                    name = std::str::from_utf8(b).map_err(|_| WireError::BadUtf8)?.to_string()
                }
                (3, FieldVal::Len(b)) => payload = b.to_vec(),
                (4, FieldVal::Varint(v)) => arg = v,
                // Strict wire-type checks for known fields; unknown fields skipped by the iter.
                (1 | 4, FieldVal::Len(_)) => {
                    return Err(WireError::BadWireType { field: num, got: WT_LEN })
                }
                (2 | 3, FieldVal::Varint(_)) => {
                    return Err(WireError::BadWireType { field: num, got: WT_VARINT })
                }
                _ => {}
            }
        }
        let op = op.ok_or(WireError::Truncated)?;
        Ok(Request { op, name, payload, arg })
    }
}

/// A reply from the broker to `libpocketforge`.
///
/// Field numbers: 1=status (varint), 2=payload (len-delim), 3=flag (varint),
/// 4=permission (varint).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: Status,
    /// Result bytes (e.g. a `GetCapability` value).
    pub payload: Vec<u8>,
    /// A small scalar result (bool 0/1 for presence; [`RumbleStatus`] for a pulse).
    pub flag: u64,
    /// The permission state for a `Query`.
    pub permission: Permission,
}

impl Response {
    /// A bare `Ok` response.
    pub fn ok() -> Response {
        Response {
            status: Status::Ok,
            payload: Vec::new(),
            flag: 0,
            permission: Permission::Granted,
        }
    }

    /// An error response carrying one of the four taxonomy statuses.
    pub fn err(status: Status) -> Response {
        Response { status, ..Response::ok() }
    }

    /// An `Ok` response whose `flag` is a boolean (presence/grant checks).
    pub fn boolean(v: bool) -> Response {
        Response { flag: v as u64, ..Response::ok() }
    }

    /// Encode to the PFW1 body.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(16 + self.payload.len());
        put_varint_field(&mut b, 1, self.status as u64);
        if !self.payload.is_empty() {
            put_len_field(&mut b, 2, &self.payload);
        }
        if self.flag != 0 {
            put_varint_field(&mut b, 3, self.flag);
        }
        if self.permission != Permission::Granted {
            put_varint_field(&mut b, 4, self.permission as u64);
        }
        b
    }

    /// Decode a PFW1 body into a response.
    pub fn decode(buf: &[u8]) -> Result<Response> {
        let mut status: Option<Status> = None;
        let mut payload = Vec::new();
        let mut flag = 0u64;
        let mut permission = Permission::Granted;
        for field in FieldIter::new(buf) {
            let (num, val) = field?;
            match (num, val) {
                (1, FieldVal::Varint(v)) => status = Some(Status::from_u64(v)?),
                (2, FieldVal::Len(b)) => payload = b.to_vec(),
                (3, FieldVal::Varint(v)) => flag = v,
                (4, FieldVal::Varint(v)) => permission = Permission::from_u64(v)?,
                (1 | 3 | 4, FieldVal::Len(_)) => {
                    return Err(WireError::BadWireType { field: num, got: WT_LEN })
                }
                (2, FieldVal::Varint(_)) => {
                    return Err(WireError::BadWireType { field: num, got: WT_VARINT })
                }
                _ => {}
            }
        }
        let status = status.ok_or(WireError::Truncated)?;
        Ok(Response { status, payload, flag, permission })
    }
}

// ---------------------------------------------------------------------------
// Framing over a byte stream.
// ---------------------------------------------------------------------------

/// Write a `u32` big-endian length prefix then `body` to `w`.
pub fn write_frame<W: Write>(w: &mut W, body: &[u8]) -> Result<()> {
    if body.len() > MAX_FRAME {
        return Err(WireError::FrameTooLarge(body.len()));
    }
    w.write_all(&(body.len() as u32).to_be_bytes())?;
    w.write_all(body)?;
    w.flush()?;
    Ok(())
}

/// Read one length-prefixed frame body from `r`. Returns the body bytes.
pub fn read_frame<R: Read>(r: &mut R) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(WireError::FrameTooLarge(len));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(body)
}

/// Convenience: encode + frame a request onto a stream.
pub fn send_request<W: Write>(w: &mut W, req: &Request) -> Result<()> {
    write_frame(w, &req.encode())
}

/// Convenience: read + decode a request from a stream.
pub fn recv_request<R: Read>(r: &mut R) -> Result<Request> {
    Request::decode(&read_frame(r)?)
}

/// Convenience: encode + frame a response onto a stream.
pub fn send_response<W: Write>(w: &mut W, resp: &Response) -> Result<()> {
    write_frame(w, &resp.encode())
}

/// Convenience: read + decode a response from a stream.
pub fn recv_response<R: Read>(r: &mut R) -> Result<Response> {
    Response::decode(&read_frame(r)?)
}

// ---------------------------------------------------------------------------
// Low-level protobuf-wire field codec.
// ---------------------------------------------------------------------------

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            break;
        }
    }
}

fn put_varint_field(out: &mut Vec<u8>, field: u64, v: u64) {
    put_varint(out, (field << 3) | WT_VARINT);
    put_varint(out, v);
}

fn put_len_field(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
    put_varint(out, (field << 3) | WT_LEN);
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// A decoded field value, borrowed from the buffer.
enum FieldVal<'a> {
    Varint(u64),
    Len(&'a [u8]),
}

/// Iterates `(field_number, value)` pairs over a PFW1 body, skipping unknown wire types
/// it can structurally parse (varint / len-delimited) and erroring on the rest.
struct FieldIter<'a> {
    buf: &'a [u8],
    pos: usize,
    done: bool,
}

impl<'a> FieldIter<'a> {
    fn new(buf: &'a [u8]) -> Self {
        FieldIter { buf, pos: 0, done: false }
    }

    fn read_varint(&mut self) -> Result<u64> {
        let mut shift = 0u32;
        let mut out = 0u64;
        for _ in 0..10 {
            let byte = *self.buf.get(self.pos).ok_or(WireError::Truncated)?;
            self.pos += 1;
            out |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(out);
            }
            shift += 7;
        }
        Err(WireError::Truncated)
    }
}

impl<'a> Iterator for FieldIter<'a> {
    type Item = Result<(u64, FieldVal<'a>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.pos >= self.buf.len() {
            return None;
        }
        let key = match self.read_varint() {
            Ok(k) => k,
            Err(e) => {
                self.done = true;
                return Some(Err(e));
            }
        };
        let field = key >> 3;
        let wire_type = key & 0x7;
        match wire_type {
            WT_VARINT => match self.read_varint() {
                Ok(v) => Some(Ok((field, FieldVal::Varint(v)))),
                Err(e) => {
                    self.done = true;
                    Some(Err(e))
                }
            },
            WT_LEN => {
                let len = match self.read_varint() {
                    Ok(l) => l as usize,
                    Err(e) => {
                        self.done = true;
                        return Some(Err(e));
                    }
                };
                let end = self.pos.checked_add(len);
                match end {
                    Some(end) if end <= self.buf.len() => {
                        let slice = &self.buf[self.pos..end];
                        self.pos = end;
                        Some(Ok((field, FieldVal::Len(slice))))
                    }
                    _ => {
                        self.done = true;
                        Some(Err(WireError::Truncated))
                    }
                }
            }
            other => {
                self.done = true;
                Some(Err(WireError::BadWireType { field, got: other }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        for v in [0u64, 1, 127, 128, 300, 16_383, 16_384, u32::MAX as u64, u64::MAX] {
            let mut b = Vec::new();
            put_varint(&mut b, v);
            let mut it = FieldIter::new(&b);
            // read raw varint via a tiny shim: re-use read_varint by peeking.
            let got = it.read_varint().unwrap();
            assert_eq!(got, v, "varint {v}");
        }
    }

    #[test]
    fn request_roundtrip_all_fields() {
        let req = Request {
            op: Op::SetCapability,
            name: "location".into(),
            payload: vec![1, 2, 3, 4, 0, 255],
            arg: 40,
        };
        let bytes = req.encode();
        assert_eq!(Request::decode(&bytes).unwrap(), req);
    }

    #[test]
    fn request_roundtrip_minimal() {
        let req = Request::new(Op::IsPresent, "imu");
        assert_eq!(Request::decode(&req.encode()).unwrap(), req);
    }

    #[test]
    fn response_roundtrip_taxonomy() {
        for st in [
            Status::Ok,
            Status::Unsupported,
            Status::PolicyBlocked,
            Status::ConsentDenied,
            Status::HardwareAbsent,
        ] {
            let resp = Response { status: st, payload: vec![9, 9], flag: 7, permission: Permission::Prompt };
            assert_eq!(Response::decode(&resp.encode()).unwrap(), resp);
        }
    }

    #[test]
    fn response_permission_default_granted_roundtrips() {
        let resp = Response::boolean(true);
        let got = Response::decode(&resp.encode()).unwrap();
        assert_eq!(got.permission, Permission::Granted);
        assert_eq!(got.flag, 1);
    }

    #[test]
    fn frame_roundtrip_over_stream() {
        let req = Request::new(Op::RumblePulse, "vibration");
        let mut buf: Vec<u8> = Vec::new();
        send_request(&mut buf, &req).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(recv_request(&mut cursor).unwrap(), req);
    }

    #[test]
    fn frame_too_large_rejected() {
        let big = vec![0u8; MAX_FRAME + 1];
        let mut out = Vec::new();
        assert!(matches!(write_frame(&mut out, &big), Err(WireError::FrameTooLarge(_))));
    }

    #[test]
    fn oversized_length_prefix_rejected_on_read() {
        // A hand-built frame claiming a huge length must be refused before allocation.
        let mut bytes = ((MAX_FRAME as u32) + 1).to_be_bytes().to_vec();
        bytes.push(0);
        let mut cursor = std::io::Cursor::new(bytes);
        assert!(matches!(read_frame(&mut cursor), Err(WireError::FrameTooLarge(_))));
    }

    #[test]
    fn unknown_field_is_skipped() {
        // Encode a request, then append an unknown field (field 9, varint) — must still decode.
        let mut bytes = Request::new(Op::Query, "audio").encode();
        put_varint_field(&mut bytes, 9, 12345);
        let got = Request::decode(&bytes).unwrap();
        assert_eq!(got.name, "audio");
        assert_eq!(got.op, Op::Query);
    }

    #[test]
    fn wrong_wire_type_for_known_field_errors() {
        // field 1 (op) as a length-delimited value must error.
        let mut bytes = Vec::new();
        put_len_field(&mut bytes, 1, b"oops");
        assert!(matches!(Request::decode(&bytes), Err(WireError::BadWireType { field: 1, .. })));
    }
}
