use pf_prefsd::{serve_until, RpcResponse};
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
        let socket = root.join("prefsd.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let state = root.join("state");
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            serve_until(
                listener,
                &pf_prefs::PrefsStore::at(state),
                unsafe { libc::geteuid() },
                &thread_stop,
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

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
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
    assert_eq!(values.len(), 7);
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
        let RpcResponse::Error { message } = rpc(&server.socket, request) else {
            panic!("corrupt state did not produce an error");
        };
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
