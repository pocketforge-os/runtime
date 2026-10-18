use pf_prefsd::{serve_until_with_timeout_and_resolver, ErrorKind, PreferenceWriter, RpcResponse};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pf-prefsd-{label}-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Server {
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(root: &Path) -> Self {
        Self::start_as(root, Some(PreferenceWriter::Shell))
    }

    fn start_as(root: &Path, writer: Option<PreferenceWriter>) -> Self {
        Self::start_with_timeout_as(root, Duration::from_millis(100), writer)
    }

    fn start_with_seed(root: &Path, seed: &Path) -> Self {
        Self::start_store(
            root,
            Duration::from_millis(100),
            Some(PreferenceWriter::Shell),
            Some(seed.to_path_buf()),
        )
    }

    fn start_with_timeout_as(
        root: &Path,
        connection_timeout: Duration,
        writer: Option<PreferenceWriter>,
    ) -> Self {
        Self::start_store(root, connection_timeout, writer, None)
    }

    fn start_store(
        root: &Path,
        connection_timeout: Duration,
        writer: Option<PreferenceWriter>,
        seed: Option<PathBuf>,
    ) -> Self {
        let socket = root.join("prefsd.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let state = root.join("state");
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut store = pf_prefs::PrefsStore::at(state);
            if let Some(seed) = seed {
                store = store.with_seed(seed);
            }
            serve_until_with_timeout_and_resolver(
                listener,
                &store,
                unsafe { libc::geteuid() },
                &thread_stop,
                connection_timeout,
                move |_| Ok(writer),
            )
            .unwrap();
        });
        Self {
            socket,
            stop,
            thread: Some(thread),
        }
    }
}

#[test]
fn seed_file_feeds_keyboard_layout_until_the_user_sets_it() {
    let root = Scratch::new("seed-layout");
    let seed = root.0.join("prefs.seed.json");
    std::fs::write(&seed, br#"{"keyboardLayout":"de"}"#).unwrap();
    let server = Server::start_with_seed(&root.0, &seed);

    // Positive control: the seeded layout is the effective value...
    assert_eq!(
        rpc(
            &server.socket,
            serde_json::json!({"method":"get", "key":"keyboardLayout"})
        ),
        RpcResponse::Value {
            value: serde_json::json!("de")
        }
    );
    // ...visible through GetAll as well...
    match rpc(&server.socket, serde_json::json!({"method":"get_all"})) {
        RpcResponse::Values { values } => {
            assert_eq!(values.get("keyboardLayout"), Some(&serde_json::json!("de")));
            // Negative control: unrelated keys keep their schema defaults.
            assert_eq!(values.get("brightness"), Some(&serde_json::json!(100)));
        }
        other => panic!("expected values, got {other:?}"),
    }
    // ...but still "First-boot setup", never "You".
    assert_eq!(
        rpc(
            &server.socket,
            serde_json::json!({"method":"is_explicit", "key":"keyboardLayout"})
        ),
        RpcResponse::Explicit { explicit: false }
    );

    // Once the user sets the key, the stored value wins over the seed.
    assert_eq!(
        rpc(
            &server.socket,
            serde_json::json!({"method":"set", "key":"keyboardLayout", "value":"us"})
        ),
        RpcResponse::Value {
            value: serde_json::json!("us")
        }
    );
    assert_eq!(
        rpc(
            &server.socket,
            serde_json::json!({"method":"is_explicit", "key":"keyboardLayout"})
        ),
        RpcResponse::Explicit { explicit: true }
    );
    // The seed never leaks into the persisted store document.
    let stored = std::fs::read_to_string(root.0.join("state").join("prefs.json")).unwrap();
    assert!(stored.contains("\"keyboardLayout\": \"us\""));
    assert!(!stored.contains("prefs.seed"));

    // Negative control: an identical daemon WITHOUT the seed serves the default.
    let bare = Scratch::new("seed-bare");
    let server = Server::start_as(&bare.0, Some(PreferenceWriter::Shell));
    assert_eq!(
        rpc(
            &server.socket,
            serde_json::json!({"method":"get", "key":"keyboardLayout"})
        ),
        RpcResponse::Value {
            value: serde_json::json!("us")
        }
    );
}

#[test]
fn corrupt_seed_file_surfaces_a_typed_store_error() {
    let root = Scratch::new("seed-corrupt");
    let seed = root.0.join("prefs.seed.json");
    std::fs::write(&seed, b"{\"keyboardLayout\":\"klingon\"}").unwrap();
    let server = Server::start_with_seed(&root.0, &seed);
    // An invalid seed must fail closed, not silently fall back to the default.
    assert!(matches!(
        rpc(
            &server.socket,
            serde_json::json!({"method":"get", "key":"keyboardLayout"})
        ),
        RpcResponse::Error {
            kind: Some(ErrorKind::InvalidValue),
            ..
        }
    ));
}

#[test]
fn same_uid_app_peer_cannot_set_but_trusted_units_can() {
    let app_root = Scratch::new("app-writer");
    let app = Server::start_as(&app_root.0, None);
    assert!(matches!(
        rpc(
            &app.socket,
            serde_json::json!({"method":"set", "key":"reduceMotion", "value":true})
        ),
        RpcResponse::Error {
            kind: Some(ErrorKind::PermissionDenied),
            ..
        }
    ));
    assert!(matches!(
        rpc(
            &app.socket,
            serde_json::json!({"method":"get", "key":"reduceMotion"})
        ),
        RpcResponse::Value { .. }
    ));

    for (label, writer) in [
        ("settings-writer", PreferenceWriter::Settings),
        ("shell-writer", PreferenceWriter::Shell),
    ] {
        let root = Scratch::new(label);
        let server = Server::start_as(&root.0, Some(writer));
        assert_eq!(
            rpc(
                &server.socket,
                serde_json::json!({"method":"set", "key":"reduceMotion", "value":true})
            ),
            RpcResponse::Value {
                value: serde_json::json!(true)
            }
        );
    }
}

#[test]
fn volume_keys_are_writable_by_control_plane_but_not_apps() {
    for (key, value) in [
        ("volumeSpeaker", serde_json::json!(35)),
        ("volumeHeadphones", serde_json::json!(45)),
        ("muted", serde_json::json!(true)),
    ] {
        let app_root = Scratch::new(&format!("app-volume-{key}"));
        let app = Server::start_as(&app_root.0, None);
        assert!(matches!(
            rpc(
                &app.socket,
                serde_json::json!({"method":"set", "key":key, "value":value})
            ),
            RpcResponse::Error {
                kind: Some(ErrorKind::PermissionDenied),
                ..
            }
        ));

        for (label, writer) in [
            ("settings", PreferenceWriter::Settings),
            ("shell", PreferenceWriter::Shell),
        ] {
            let root = Scratch::new(&format!("{label}-volume-{key}"));
            let server = Server::start_as(&root.0, Some(writer));
            assert_eq!(
                rpc(
                    &server.socket,
                    serde_json::json!({"method":"set", "key":key, "value":value})
                ),
                RpcResponse::Value {
                    value: value.clone()
                }
            );
        }
    }
}

#[test]
fn idle_client_times_out_and_does_not_block_next_client() {
    let scratch = Scratch::new("idle-timeout");
    let server = Server::start(&scratch.0);
    let _stalled = UnixStream::connect(&server.socket).unwrap();

    let started = Instant::now();
    assert!(matches!(
        rpc(
            &server.socket,
            serde_json::json!({"method":"get", "key":"monoAudio"})
        ),
        RpcResponse::Value { .. }
    ));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn partial_length_prefix_times_out_and_does_not_block_next_client() {
    let scratch = Scratch::new("partial-timeout");
    let server = Server::start(&scratch.0);
    let mut stalled = UnixStream::connect(&server.socket).unwrap();
    stalled.write_all(&[0, 0]).unwrap();

    let started = Instant::now();
    assert!(matches!(
        rpc(
            &server.socket,
            serde_json::json!({"method":"get", "key":"monoAudio"})
        ),
        RpcResponse::Value { .. }
    ));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn stop_is_observed_after_stalled_client_times_out() {
    let scratch = Scratch::new("stalled-stop");
    let mut server = Server::start(&scratch.0);
    let _stalled = UnixStream::connect(&server.socket).unwrap();
    std::thread::sleep(Duration::from_millis(25));

    let started = Instant::now();
    server.stop.store(true, Ordering::Release);
    server.thread.take().unwrap().join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn rpc(socket: &Path, request: serde_json::Value) -> RpcResponse {
    let mut stream = UnixStream::connect(socket).unwrap();
    pf_wire::write_frame(&mut stream, &serde_json::to_vec(&request).unwrap()).unwrap();
    let body = pf_wire::read_frame(&mut stream).unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[test]
fn defaults_set_get_all_and_persistence_use_schema_v2() {
    let scratch = Scratch::new("round-trip");
    let server = Server::start(&scratch.0);

    assert_eq!(
        rpc(
            &server.socket,
            serde_json::json!({"method":"get", "key":"highContrast"})
        ),
        RpcResponse::Value {
            value: serde_json::json!(false)
        }
    );
    assert_eq!(
        rpc(
            &server.socket,
            serde_json::json!({"method":"get", "key":"brightness"})
        ),
        RpcResponse::Value {
            value: serde_json::json!(100)
        }
    );
    assert_eq!(
        rpc(
            &server.socket,
            serde_json::json!({"method":"get", "key":"textScale"})
        ),
        RpcResponse::Value {
            value: serde_json::json!("100%")
        }
    );
    assert_eq!(
        rpc(
            &server.socket,
            serde_json::json!({"method":"set", "key":"brightness", "value":40})
        ),
        RpcResponse::Value {
            value: serde_json::json!(40)
        }
    );
    assert_eq!(
        rpc(
            &server.socket,
            serde_json::json!({"method":"get", "key":"brightness"})
        ),
        RpcResponse::Value {
            value: serde_json::json!(40)
        }
    );
    let RpcResponse::Values { values } =
        rpc(&server.socket, serde_json::json!({"method":"get_all"}))
    else {
        panic!("get_all did not return values");
    };
    assert_eq!(values.len(), pf_prefs::SCHEMA.len());
    assert_eq!(values["appearance"], serde_json::json!("dark"));
    assert_eq!(values["hapticsEnabled"], serde_json::json!(true));

    let document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(scratch.0.join("state/prefs.json")).unwrap())
            .unwrap();
    assert_eq!(document["schemaVersion"], 2);
    assert_eq!(document["brightness"], 40);
}

#[test]
fn invalid_sets_do_not_change_the_store() {
    let scratch = Scratch::new("invalid");
    let server = Server::start(&scratch.0);
    let invalid = [
        serde_json::json!({"method":"set", "key":"unknown", "value":true}),
        serde_json::json!({"method":"set", "key":"textScale", "value":true}),
        serde_json::json!({"method":"set", "key":"brightness", "value":101}),
    ];
    for request in invalid {
        assert!(matches!(
            rpc(&server.socket, request),
            RpcResponse::Error { .. }
        ));
    }
    assert!(!scratch.0.join("state/prefs.json").exists());
}

#[test]
fn invalid_enum_is_classified_as_invalid_value() {
    let scratch = Scratch::new("invalid-enum");
    let server = Server::start(&scratch.0);
    let response = rpc(
        &server.socket,
        serde_json::json!({"method":"set", "key":"textScale", "value":"110%"}),
    );
    assert!(matches!(
        response,
        RpcResponse::Error {
            kind: Some(ErrorKind::InvalidValue),
            ..
        }
    ));
}

#[test]
fn bad_requests_do_not_stop_the_daemon() {
    let scratch = Scratch::new("bad-request");
    let server = Server::start(&scratch.0);
    assert!(matches!(
        rpc(&server.socket, serde_json::json!({"method":"not_a_method"})),
        RpcResponse::Error { .. }
    ));

    let mut oversized = UnixStream::connect(&server.socket).unwrap();
    oversized
        .write_all(&((pf_wire::MAX_FRAME + 1) as u32).to_be_bytes())
        .unwrap();
    drop(oversized);

    assert!(matches!(
        rpc(
            &server.socket,
            serde_json::json!({"method":"get", "key":"monoAudio"})
        ),
        RpcResponse::Value { .. }
    ));
}

#[test]
fn corrupt_store_errors_are_surfaced_for_get_and_set() {
    let scratch = Scratch::new("corrupt");
    std::fs::create_dir_all(scratch.0.join("state")).unwrap();
    std::fs::write(scratch.0.join("state/prefs.json"), b"{bad json").unwrap();
    let server = Server::start(&scratch.0);
    for request in [
        serde_json::json!({"method":"get", "key":"brightness"}),
        serde_json::json!({"method":"set", "key":"brightness", "value":40}),
    ] {
        let RpcResponse::Error { message, kind } = rpc(&server.socket, request) else {
            panic!("corrupt state did not produce an error");
        };
        assert_eq!(kind, Some(ErrorKind::Store));
        assert!(message.contains("not a valid JSON object"));
    }
}

#[test]
fn sigterm_removes_socket_and_socket_is_owner_only() {
    let scratch = Scratch::new("sigterm");
    let state = scratch.0.join("state");
    let socket = scratch.0.join("prefsd.sock");
    let mut child = Command::new(env!("CARGO_BIN_EXE_pf-prefsd"))
        .args(["--state-dir", state.to_str().unwrap(), "--socket"])
        .arg(&socket)
        .args(["--writer-group", "root"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(socket.exists(), "daemon did not create its socket");
    assert_eq!(
        std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    assert!(child.wait().unwrap().success());
    assert!(!socket.exists(), "socket guard did not remove socket");
}

#[test]
fn first_start_creates_missing_state_and_socket_parents() {
    let scratch = Scratch::new("first-start");
    let state = scratch.0.join("state/missing");
    let socket = scratch.0.join("run/missing/prefsd.sock");
    let mut child = Command::new(env!("CARGO_BIN_EXE_pf-prefsd"))
        .args(["--state-dir", state.to_str().unwrap(), "--socket"])
        .arg(&socket)
        .args(["--writer-group", "root"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !socket.exists() && Instant::now() < deadline {
        assert!(
            child.try_wait().unwrap().is_none(),
            "daemon exited on first start"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(state.is_dir(), "daemon did not create its state directory");
    assert!(socket.exists(), "daemon did not create its socket");
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    assert!(child.wait().unwrap().success());
}
