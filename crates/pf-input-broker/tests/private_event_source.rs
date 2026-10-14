//! Device-free proof for the broker's private event seam.
//!
//! Every descriptor in this test is process-private. Nothing opens an evdev or uinput node.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::Command;
use std::sync::atomic::AtomicBool;

use pf_input_broker::{
    EventSink, EventSource, FdEventSource, InputBroker, RecordingSink, RunConfig, SourceState,
    TokenBucket,
};
use pocketforge::Descriptor;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_MSC: u16 = 4;
const SYN_REPORT: u16 = 0;
const BTN_X: u16 = 0x133;
const BTN_WEST: u16 = 0x134;

fn descriptor() -> Descriptor {
    Descriptor::from_toml(
        r#"
[identity]
id = "private-seam"
manufacturer = "PocketForge"
model = "Private seam fixture"
sdl_guid = "030000005e0400008e02000010010000"

[[inputs]]
id = "west"
kind = "button"
ev_type = "EV_KEY"
code = "BTN_X"
"#,
    )
    .unwrap()
}

fn event(event_type: u16, code: u16, value: i32) -> libc::input_event {
    // SAFETY: input_event is a plain C record and every field is initialized below.
    let mut event: libc::input_event = unsafe { std::mem::zeroed() };
    event.type_ = event_type;
    event.code = code;
    event.value = value;
    event
}

fn event_bytes(events: &[libc::input_event]) -> &[u8] {
    // SAFETY: input_event has no references; the returned byte view has the slice's lifetime.
    unsafe {
        std::slice::from_raw_parts(events.as_ptr().cast::<u8>(), std::mem::size_of_val(events))
    }
}

fn seqpacket_pair() -> (OwnedFd, OwnedFd) {
    let mut fds = [-1; 2];
    // SAFETY: fds points to storage for the two descriptors socketpair writes.
    let rc = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    assert_eq!(rc, 0, "socketpair: {}", io::Error::last_os_error());
    // SAFETY: successful socketpair returned two new, independently owned descriptors.
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

fn write_packet(fd: &OwnedFd, bytes: &[u8]) {
    let mut file = std::fs::File::from(fd.try_clone().unwrap());
    file.write_all(bytes).unwrap();
}

fn inherit_on_exec(fd: &OwnedFd) {
    // SAFETY: fd is valid; F_SETFD changes only its close-on-exec flag.
    let rc = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, 0) };
    assert_eq!(rc, 0, "fcntl(F_SETFD): {}", io::Error::last_os_error());
}

#[test]
fn fragmented_private_stream_runs_real_remap_and_policy_loop() {
    let (read_fd, write_fd) = seqpacket_pair();
    let input = [
        event(EV_KEY, BTN_X, 1),
        event(EV_SYN, SYN_REPORT, 0),
        // Negative control: an event type outside the descriptor-controlled surface is dropped.
        event(EV_MSC, 0, 99),
        event(EV_SYN, SYN_REPORT, 0),
    ];
    let bytes = event_bytes(&input);
    write_packet(&write_fd, &bytes[..7]);
    write_packet(&write_fd, &bytes[7..]);
    drop(write_fd);

    let mut source = FdEventSource::from_owned_fd(read_fd);
    let mut sink = RecordingSink::default();
    let stop = AtomicBool::new(false);
    let descriptor = descriptor();
    let config = RunConfig::new(&descriptor, &stop).with_token_bucket(TokenBucket::new(1.0, 0.0));
    let error = InputBroker::run_with_io(&mut source, &mut sink, config).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(
        sink.events()
            .iter()
            .map(|event| (event.type_, event.code, event.value))
            .collect::<Vec<_>>(),
        vec![
            (EV_KEY, BTN_WEST, 1),
            (EV_SYN, SYN_REPORT, 0),
            (EV_KEY, BTN_WEST, 0),
            (EV_SYN, SYN_REPORT, 0),
        ],
        "the real descriptor remap admits the positive report, then policy rejects the negative report and emits only the fail-safe release"
    );
}

#[test]
fn fd_source_distinguishes_clean_eof_partial_eof_and_read_failure() {
    let (clean_read, clean_write) = seqpacket_pair();
    drop(clean_write);
    let mut clean = FdEventSource::from_owned_fd(clean_read);
    let mut out: [libc::input_event; 2] = unsafe { std::mem::zeroed() };
    assert_eq!(
        clean.read_events(&mut out).unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );

    let (partial_read, partial_write) = seqpacket_pair();
    write_packet(
        &partial_write,
        &event_bytes(&[event(EV_KEY, BTN_X, 1)])[..7],
    );
    drop(partial_write);
    let mut partial = FdEventSource::from_owned_fd(partial_read);
    assert_eq!(
        partial.read_events(&mut out).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );

    let directory: OwnedFd = std::fs::File::open(".").unwrap().into();
    let mut failing = FdEventSource::from_owned_fd(directory);
    let error = failing.read_events(&mut out).unwrap_err();
    assert_ne!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_ne!(error.kind(), io::ErrorKind::InvalidData);
    assert!(
        error.raw_os_error().is_some(),
        "the syscall error is preserved"
    );
}

struct MockEvdevAdapter {
    batches: VecDeque<io::Result<Vec<libc::input_event>>>,
    reads: usize,
}

impl EventSource for MockEvdevAdapter {
    fn read_events(&mut self, out: &mut [libc::input_event]) -> io::Result<usize> {
        self.reads += 1;
        let events = self
            .batches
            .pop_front()
            .expect("scripted source exhausted")?;
        out[..events.len()].copy_from_slice(&events);
        Ok(events.len())
    }
}

impl SourceState for MockEvdevAdapter {
    fn pressed_keys(&self, _codes: &[u16]) -> io::Result<Vec<u16>> {
        Ok(Vec::new())
    }

    fn abs_value(&self, _code: u16) -> io::Result<i32> {
        Ok(0)
    }
}

#[derive(Default)]
struct MockUinputAdapter {
    writes: usize,
    events: Vec<libc::input_event>,
}

impl EventSink for MockUinputAdapter {
    fn write_events(&mut self, events: &[libc::input_event]) -> io::Result<()> {
        self.writes += 1;
        self.events.extend_from_slice(events);
        Ok(())
    }
}

#[test]
fn mocked_production_adapters_use_the_same_run_with_io_loop() {
    let mut source = MockEvdevAdapter {
        batches: VecDeque::from([
            Ok(vec![event(EV_KEY, BTN_X, 1), event(EV_SYN, SYN_REPORT, 0)]),
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "mock unplug")),
        ]),
        reads: 0,
    };
    let mut sink = MockUinputAdapter::default();
    let stop = AtomicBool::new(false);

    let error =
        InputBroker::run_with_io(&mut source, &mut sink, RunConfig::new(&descriptor(), &stop))
            .unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(source.reads, 2);
    assert_eq!(sink.writes, 1);
    assert_eq!(sink.events[0].code, BTN_WEST);
}

#[test]
fn binary_private_fd_mode_uses_only_inherited_streams() {
    let descriptor_path = std::env::temp_dir().join(format!(
        "pf-input-broker-private-{}.toml",
        std::process::id()
    ));
    std::fs::write(
        &descriptor_path,
        r#"
[identity]
id = "private-seam-bin"
manufacturer = "PocketForge"
model = "Private seam binary fixture"
sdl_guid = "030000005e0400008e02000010010000"

[[inputs]]
id = "west"
kind = "button"
ev_type = "EV_KEY"
code = "BTN_X"
"#,
    )
    .unwrap();

    let (event_read, event_write) = seqpacket_pair();
    let (record_read, record_write) = seqpacket_pair();
    inherit_on_exec(&event_read);
    inherit_on_exec(&record_write);
    let child = Command::new(env!("CARGO_BIN_EXE_pf-input-broker"))
        .arg("--descriptor")
        .arg(&descriptor_path)
        .arg("--test-event-fd")
        .arg(event_read.as_raw_fd().to_string())
        .arg("--test-record-fd")
        .arg(record_write.as_raw_fd().to_string())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    drop(event_read);
    drop(record_write);

    let input = [event(EV_KEY, BTN_X, 1), event(EV_SYN, SYN_REPORT, 0)];
    let bytes = event_bytes(&input);
    write_packet(&event_write, &bytes[..5]);
    write_packet(&event_write, &bytes[5..]);
    drop(event_write);

    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "private binary failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(String::from_utf8(result.stdout).unwrap(), "ready\n");

    let mut recorded = [0u8; 4096];
    // SAFETY: recorded is writable and record_read remains live.
    let count = unsafe {
        libc::read(
            record_read.as_raw_fd(),
            recorded.as_mut_ptr().cast::<libc::c_void>(),
            recorded.len(),
        )
    };
    assert!(count > 0, "record fd: {}", io::Error::last_os_error());
    let expected = [event(EV_KEY, BTN_WEST, 1), event(EV_SYN, SYN_REPORT, 0)];
    assert_eq!(&recorded[..count as usize], event_bytes(&expected));
    std::fs::remove_file(descriptor_path).unwrap();
}
