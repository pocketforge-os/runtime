//! Shared harness for pf-app-fetch's hermetic tests (tsp-ziac.4.3.5).
//!
//! - [`Server`]: an in-process rustls loopback HTTPS server with scripted routes, keyed by
//!   (SNI host, path), that counts every request.
//! - [`Tar`]: a hand-written ustar builder, so the unsafe-member cases (`..`, absolute,
//!   links, devices, FIFO, duplicates) can be expressed at all.
//! - [`Fixture`]: a temp app root with a descriptor, plus [`Fixture::run`], which executes
//!   the real binary with the debug-build test hooks pointed at the server.
//! - [`w4_state`]: the stat-only Ready check W4 will run (§4.6 Validity), so every test can
//!   assert NeedsSetup vs Ready the way the launcher will see it.
#![allow(dead_code)]

use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const APP_ID: &str = "org.pocketforge.fetchfixture";

fn tls_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tls")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------------------
// Loopback HTTPS server
// ---------------------------------------------------------------------------------------

#[derive(Clone)]
pub enum Reply {
    /// A complete response with `Content-Length`.
    Body { status: u16, body: Vec<u8> },
    /// A complete 200 body with no `Content-Length` (connection close delimits it).
    NoLength { body: Vec<u8> },
    /// Announce `content_length`, send `body`, then close early.
    Truncate { content_length: u64, body: Vec<u8> },
    /// A redirect with an optional `Location`.
    Redirect {
        status: u16,
        location: Option<String>,
    },
    /// Announce `content_length`, send `body`, then stall for `stall`.
    Stall {
        content_length: u64,
        body: Vec<u8>,
        stall: Duration,
    },
}

pub struct Server {
    pub port: u16,
    routes: Arc<Mutex<HashMap<(String, String), Reply>>>,
    requests: Arc<AtomicUsize>,
    log: Arc<Mutex<Vec<(String, String)>>>,
}

impl Server {
    pub fn start() -> Server {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let cert = rustls::pki_types::CertificateDer::from(
            fs::read(tls_dir().join("leaf.der")).expect("leaf cert"),
        );
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
            fs::read(tls_dir().join("leaf.key.der"))
                .expect("leaf key")
                .into(),
        );
        let config = Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert], key)
                .expect("server config"),
        );
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        let routes: Arc<Mutex<HashMap<(String, String), Reply>>> = Arc::default();
        let requests = Arc::new(AtomicUsize::new(0));
        let log: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        {
            let routes = routes.clone();
            let requests = requests.clone();
            let log = log.clone();
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    let config = config.clone();
                    let routes = routes.clone();
                    let requests = requests.clone();
                    let log = log.clone();
                    thread::spawn(move || {
                        let _ = serve(stream, config, routes, requests, log);
                    });
                }
            });
        }
        Server {
            port,
            routes,
            requests,
            log,
        }
    }

    pub fn route(&self, host: &str, path: &str, reply: Reply) {
        self.routes
            .lock()
            .unwrap()
            .insert((host.to_string(), path.to_string()), reply);
    }

    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    pub fn log(&self) -> Vec<(String, String)> {
        self.log.lock().unwrap().clone()
    }

    pub fn reset_count(&self) {
        self.requests.store(0, Ordering::SeqCst);
        self.log.lock().unwrap().clear();
    }
}

fn serve(
    tcp: TcpStream,
    config: Arc<rustls::ServerConfig>,
    routes: Arc<Mutex<HashMap<(String, String), Reply>>>,
    requests: Arc<AtomicUsize>,
    log: Arc<Mutex<Vec<(String, String)>>>,
) -> std::io::Result<()> {
    tcp.set_read_timeout(Some(Duration::from_secs(20)))?;
    let conn = rustls::ServerConnection::new(config).map_err(std::io::Error::other)?;
    let mut tls = rustls::StreamOwned::new(conn, tcp);
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if tls.read(&mut byte)? == 0 {
            return Ok(());
        }
        head.push(byte[0]);
        if head.len() > 16 * 1024 {
            return Ok(());
        }
    }
    let host = tls.conn.server_name().unwrap_or_default().to_string();
    let text = String::from_utf8_lossy(&head).to_string();
    let path = text
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string();
    requests.fetch_add(1, Ordering::SeqCst);
    log.lock().unwrap().push((host.clone(), path.clone()));
    let reply = routes.lock().unwrap().get(&(host, path)).cloned();
    let reply = reply.unwrap_or(Reply::Body {
        status: 404,
        body: b"no route".to_vec(),
    });
    let reason = |status: u16| match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Status",
    };
    match reply {
        Reply::Body { status, body } => {
            write!(
                tls,
                "HTTP/1.1 {status} {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                reason(status),
                body.len()
            )?;
            tls.write_all(&body)?;
        }
        Reply::NoLength { body } => {
            write!(tls, "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")?;
            tls.write_all(&body)?;
        }
        Reply::Truncate {
            content_length,
            body,
        } => {
            write!(
                tls,
                "HTTP/1.1 200 OK\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
            )?;
            tls.write_all(&body)?;
        }
        Reply::Redirect { status, location } => {
            write!(tls, "HTTP/1.1 {status} Redirect\r\n")?;
            if let Some(location) = location {
                write!(tls, "Location: {location}\r\n")?;
            }
            write!(tls, "Content-Length: 0\r\nConnection: close\r\n\r\n")?;
        }
        Reply::Stall {
            content_length,
            body,
            stall,
        } => {
            write!(
                tls,
                "HTTP/1.1 200 OK\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
            )?;
            tls.write_all(&body)?;
            tls.flush()?;
            thread::sleep(stall);
        }
    }
    tls.flush()?;
    tls.conn.send_close_notify();
    let _ = tls.flush();
    Ok(())
}

/// A loopback port with nothing listening (bound, then released).
pub fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

// ---------------------------------------------------------------------------------------
// ustar builder
// ---------------------------------------------------------------------------------------

#[derive(Default)]
pub struct Tar {
    bytes: Vec<u8>,
}

impl Tar {
    pub fn new() -> Tar {
        Tar::default()
    }

    fn header(&mut self, name: &str, typeflag: u8, size: usize, linkname: &str) {
        assert!(name.len() <= 100, "fixture names fit the ustar name field");
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        let mode = if typeflag == b'5' {
            "0000755"
        } else {
            "0000644"
        };
        h[100..107].copy_from_slice(mode.as_bytes());
        h[108..115].copy_from_slice(b"0001750");
        h[116..123].copy_from_slice(b"0001750");
        let size_field = format!("{size:011o}");
        h[124..135].copy_from_slice(size_field.as_bytes());
        h[136..147].copy_from_slice(b"14737432000");
        h[148..156].copy_from_slice(b"        ");
        h[156] = typeflag;
        h[157..157 + linkname.len()].copy_from_slice(linkname.as_bytes());
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        let sum_field = format!("{sum:06o}\0 ");
        h[148..156].copy_from_slice(sum_field.as_bytes());
        self.bytes.extend_from_slice(&h);
    }

    pub fn file(mut self, name: &str, body: &[u8]) -> Tar {
        self.header(name, b'0', body.len(), "");
        self.bytes.extend_from_slice(body);
        let pad = (512 - body.len() % 512) % 512;
        self.bytes.extend(std::iter::repeat_n(0u8, pad));
        self
    }

    pub fn dir(mut self, name: &str) -> Tar {
        self.header(name, b'5', 0, "");
        self
    }

    pub fn symlink(mut self, name: &str, target: &str) -> Tar {
        self.header(name, b'2', 0, target);
        self
    }

    pub fn hardlink(mut self, name: &str, target: &str) -> Tar {
        self.header(name, b'1', 0, target);
        self
    }

    pub fn special(mut self, name: &str, typeflag: u8) -> Tar {
        self.header(name, typeflag, 0, "");
        self
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.bytes.extend(std::iter::repeat_n(0u8, 1024));
        self.bytes
    }

    pub fn gz(self) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&self.finish()).unwrap();
        encoder.finish().unwrap()
    }
}

/// The good archive: a release-style top directory, a `data/` subtree and an unrelated
/// binary-ish member outside it (left out by `include`).
pub fn good_archive(version: &str) -> Vec<u8> {
    Tar::new()
        .dir("Rel/")
        .dir("Rel/data/")
        .file("Rel/data/a.txt", format!("alpha {version}\n").as_bytes())
        .dir("Rel/data/sub/")
        .file("Rel/data/sub/b.bin", &[7u8; 3000])
        .file("Rel/run.sh", b"#!/bin/sh\n")
        .symlink("Rel/libfoo.so", "libfoo.so.1")
        .gz()
}

/// What `good_archive(version)` extracts to under `strip_components = 1` and
/// `include = ["Rel/data/"]`.
pub fn good_payload(version: &str) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    files.insert(
        "assets/data/a.txt".to_string(),
        format!("alpha {version}\n").into_bytes(),
    );
    files.insert("assets/data/sub/b.bin".to_string(), vec![7u8; 3000]);
    files
}

// ---------------------------------------------------------------------------------------
// Fixture: descriptor + state dir + binary invocation
// ---------------------------------------------------------------------------------------

pub struct FileSpec {
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub format: String,
    pub dest: String,
    pub strip: Option<u64>,
    pub include: Option<Vec<String>>,
}

impl FileSpec {
    pub fn for_bytes(url: &str, bytes: &[u8]) -> FileSpec {
        FileSpec {
            url: url.to_string(),
            sha256: sha256_hex(bytes),
            size: bytes.len() as u64,
            format: "tar.gz".into(),
            dest: "assets".into(),
            strip: Some(1),
            include: Some(vec!["Rel/data/".into()]),
        }
    }
}

pub fn descriptor(files: &[FileSpec], reason: &str, extra: &str) -> String {
    let mut text = format!(
        "schema_version = 3\n\n[app]\nid = \"{APP_ID}\"\nname = \"Fetch fixture\"\ncategory = \"game\"\nversion = \"1\"\nuse = []\n\n[runtime]\nfamily = \"pocketforge/a133-powervr\"\nabi = \"1\"\nplatform-version = \"20\"\n\n[launch]\nexec = \"launch\"\n\n[fetch]\nenabled = true\nreason = \"{reason}\"\n{extra}\n"
    );
    for file in files {
        text.push_str(&format!(
            "[[fetch.files]]\nurl = \"{}\"\nsha256 = \"{}\"\nsize = {}\nformat = \"{}\"\ndest = \"{}\"\n",
            file.url, file.sha256, file.size, file.format, file.dest
        ));
        if let Some(strip) = file.strip {
            text.push_str(&format!("strip_components = {strip}\n"));
        }
        if let Some(include) = &file.include {
            let list: Vec<String> = include.iter().map(|i| format!("\"{i}\"")).collect();
            text.push_str(&format!("include = [{}]\n", list.join(", ")));
        }
        text.push('\n');
    }
    text
}

pub struct Fixture {
    pub root: PathBuf,
    pub state: PathBuf,
    pub descriptor: PathBuf,
}

pub struct Run {
    pub output: Output,
    pub result: Option<Value>,
}

impl Run {
    pub fn reason(&self) -> Option<String> {
        self.result
            .as_ref()
            .and_then(|r| r["reason"].as_str().map(str::to_string))
    }
    pub fn outcome(&self) -> Option<String> {
        self.result
            .as_ref()
            .and_then(|r| r["outcome"].as_str().map(str::to_string))
    }
    pub fn field(&self, name: &str) -> Value {
        self.result
            .as_ref()
            .map(|r| r[name].clone())
            .unwrap_or(Value::Null)
    }
    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).to_string()
    }
    /// Assert a failed run with exactly `code`, reported both in result.json and as the
    /// `reason=<code>` line.
    pub fn assert_failed(&self, code: &str) {
        assert_eq!(
            self.outcome().as_deref(),
            Some("failed"),
            "stderr: {}",
            self.stderr()
        );
        assert_eq!(
            self.reason().as_deref(),
            Some(code),
            "stderr: {}",
            self.stderr()
        );
        assert!(
            self.stderr().contains(&format!("reason={code}")),
            "stderr lacks reason={code}: {}",
            self.stderr()
        );
        assert_eq!(self.output.status.code(), Some(1));
    }
    pub fn assert_ready(&self) {
        assert_eq!(
            self.outcome().as_deref(),
            Some("ready"),
            "stderr: {}",
            self.stderr()
        );
        assert!(self.output.status.success(), "stderr: {}", self.stderr());
    }
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

impl Fixture {
    pub fn new(descriptor_text: &str) -> Fixture {
        let base = std::env::temp_dir().join(format!(
            "pf-app-fetch-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&base);
        let state = base.join("apps").join(APP_ID);
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let descriptor = base.join("app.toml");
        fs::write(&descriptor, descriptor_text).unwrap();
        Fixture {
            root: base,
            state,
            descriptor,
        }
    }

    pub fn set_descriptor(&self, text: &str) {
        fs::write(&self.descriptor, text).unwrap();
    }

    pub fn command(&self, server: &Server) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pf-app-fetch"));
        command
            .arg("--app-id")
            .arg(APP_ID)
            .arg("--descriptor")
            .arg(&self.descriptor)
            .arg("--state-dir")
            .arg(&self.state)
            .env("PF_APP_FETCH_TEST_CA", tls_dir().join("ca.der"))
            .env(
                "PF_APP_FETCH_TEST_RESOLVE",
                format!(
                    "fixture.test=127.0.0.1:{p},cdn.fixture.test=127.0.0.1:{p},off.fixture.test=127.0.0.1:{p}",
                    p = server.port
                ),
            )
            .env("PF_APP_FETCH_TEST_REDIRECTS", "fixture.test>cdn.fixture.test");
        command
    }

    pub fn run_with(&self, server: &Server, edit: impl FnOnce(&mut Command)) -> Run {
        let mut command = self.command(server);
        edit(&mut command);
        let output = command.output().expect("run pf-app-fetch");
        Run {
            output,
            result: self.result(),
        }
    }

    pub fn run(&self, server: &Server) -> Run {
        self.run_with(server, |_| {})
    }

    pub fn result(&self) -> Option<Value> {
        let text = fs::read_to_string(self.state.join(".pf-fetch/result.json")).ok()?;
        Some(serde_json::from_str(&text).expect("result.json is JSON"))
    }

    pub fn receipt(&self) -> Option<Value> {
        let text = fs::read_to_string(self.state.join(".pf-fetch/receipt.json")).ok()?;
        Some(serde_json::from_str(&text).expect("receipt.json is JSON"))
    }

    /// Every regular file under the app root except `.pf-fetch/`, by relative path.
    pub fn payload(&self) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        walk(&self.state, &self.state, &mut out);
        out
    }

    /// No staging, old or temp leftovers once a run has finished.
    pub fn assert_no_leftovers(&self) {
        let dir = self.state.join(".pf-fetch");
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries {
                let name = entry.unwrap().file_name().to_string_lossy().to_string();
                assert!(
                    matches!(name.as_str(), "receipt.json" | "result.json" | "lock"),
                    "leftover .pf-fetch/{name}"
                );
            }
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        let rel = path
            .strip_prefix(base)
            .unwrap()
            .to_string_lossy()
            .to_string();
        if rel == ".pf-fetch" {
            continue;
        }
        let meta = fs::symlink_metadata(&path).unwrap();
        if meta.is_dir() {
            assert_eq!(meta.mode() & 0o7777, 0o755, "{rel} is a 0755 directory");
            walk(&path, base, out);
        } else {
            assert!(meta.is_file(), "{rel} is a regular file");
            assert_eq!(meta.mode() & 0o7777, 0o644, "{rel} is a 0644 file");
            out.insert(rel, fs::read(&path).unwrap());
        }
    }
}

// ---------------------------------------------------------------------------------------
// The W4 stat-only check (§4.6 Validity)
// ---------------------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum W4 {
    Ready,
    NeedsSetup(&'static str),
}

fn stat_walk(dir: &Path, count: &mut u64, bytes: &mut u64) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            return false;
        };
        if meta.is_dir() {
            if !stat_walk(&path, count, bytes) {
                return false;
            }
        } else if meta.is_file() {
            *count += 1;
            *bytes += meta.len();
        } else {
            return false;
        }
    }
    true
}

/// What the launcher (W4) will decide, from the receipt and stat calls only.
pub fn w4_state(fixture: &Fixture) -> W4 {
    let descriptor = fs::read_to_string(&fixture.descriptor).unwrap();
    let manifest = pf_app_manifest::parse_manifest(&descriptor).expect("descriptor parses");
    let fetch = manifest.fetch.expect("fetch block");
    let Some(receipt) = fixture.receipt() else {
        return W4::NeedsSetup("absent");
    };
    let key = pf_app_manifest::fetch_receipt_key(&fetch).expect("enabled block has a key");
    if receipt["key"].as_str() != Some(key.as_str()) {
        return W4::NeedsSetup("receipt_stale");
    }
    let Some(files) = receipt["files"].as_array() else {
        return W4::NeedsSetup("state_invalid");
    };
    for file in fetch.files.unwrap_or_default() {
        let dest = file.dest.unwrap();
        let Some(entry) = files.iter().find(|f| f["dest"].as_str() == Some(&dest)) else {
            return W4::NeedsSetup("state_invalid");
        };
        let dir = fixture.state.join(&dest);
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => {}
            _ => return W4::NeedsSetup("state_invalid"),
        }
        let (mut count, mut bytes) = (0, 0);
        if !stat_walk(&dir, &mut count, &mut bytes)
            || entry["file_count"].as_u64() != Some(count)
            || entry["total_bytes"].as_u64() != Some(bytes)
        {
            return W4::NeedsSetup("state_invalid");
        }
    }
    W4::Ready
}

/// The full check: W4 Ready, and the payload is byte-for-byte `expected`.
pub fn assert_ready_with(fixture: &Fixture, expected: &BTreeMap<String, Vec<u8>>) {
    assert_eq!(w4_state(fixture), W4::Ready);
    let payload = fixture.payload();
    assert_eq!(
        payload.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>()
    );
    assert_eq!(&payload, expected);
}
