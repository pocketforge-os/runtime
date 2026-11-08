//! Loopback HTTPS fixture cases for pf-app-fetch (tsp-ziac.4.3.5 acceptance). Every
//! failure asserts the exact W0 reason code from mission-control
//! `.planning/infra/infra-107.4-fetch-reason-codes.toml` (plus W3's `source_moved` class).

mod common;

use common::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

const URL: &str = "https://fixture.test/rel/a.tar.gz";
const PATH: &str = "/rel/a.tar.gz";

fn good(server: &Server) -> (Fixture, Vec<u8>) {
    let archive = good_archive("v1");
    server.route(
        "fixture.test",
        PATH,
        Reply::Body {
            status: 200,
            body: archive.clone(),
        },
    );
    let fixture = Fixture::new(&descriptor(&[FileSpec::for_bytes(URL, &archive)], "r", ""));
    (fixture, archive)
}

#[test]
fn good_install_stages_payload_receipt_and_result() {
    let server = Server::start();
    let (fixture, archive) = good(&server);
    let run = fixture.run(&server);
    run.assert_ready();
    assert_eq!(server.requests(), 1);
    assert_ready_with(&fixture, &good_payload("v1"));
    fixture.assert_no_leftovers();
    let receipt = fixture.receipt().unwrap();
    assert_eq!(receipt["files"][0]["dest"], "assets");
    assert_eq!(receipt["files"][0]["archive_sha256"], sha256_hex(&archive));
    assert_eq!(receipt["files"][0]["file_count"], 2);
    assert_eq!(
        receipt["files"][0]["total_bytes"],
        3000 + "alpha v1\n".len()
    );
    let manifest = receipt["files"][0]["manifest"].as_array().unwrap();
    assert_eq!(manifest[0]["path"], "data/a.txt");
    assert_eq!(manifest[0]["sha256"], sha256_hex(b"alpha v1\n"));
    assert_eq!(manifest[1]["path"], "data/sub/b.bin");
    // §4.6 ownership: .pf-fetch 0755, receipt/result 0644 (launcher-readable).
    let mode = |p: &str| {
        fs::metadata(fixture.state.join(p))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    };
    assert_eq!(mode(".pf-fetch"), 0o755);
    assert_eq!(mode(".pf-fetch/receipt.json"), 0o644);
    assert_eq!(mode(".pf-fetch/result.json"), 0o644);
    assert_eq!(run.field("check"), "absent");
    assert_eq!(run.field("schema_version"), 1);
}

#[test]
fn plain_tar_format_installs() {
    let server = Server::start();
    let archive = common::Tar::new()
        .dir("Rel/")
        .file("Rel/x.txt", b"x")
        .finish();
    server.route(
        "fixture.test",
        PATH,
        Reply::Body {
            status: 200,
            body: archive.clone(),
        },
    );
    let mut spec = FileSpec::for_bytes(URL, &archive);
    spec.format = "tar".into();
    spec.include = None;
    let fixture = Fixture::new(&descriptor(&[spec], "r", ""));
    fixture.run(&server).assert_ready();
    let mut want = std::collections::BTreeMap::new();
    want.insert("assets/x.txt".to_string(), b"x".to_vec());
    assert_ready_with(&fixture, &want);
}

#[test]
fn rerun_with_valid_receipt_makes_zero_gets() {
    let server = Server::start();
    let (fixture, _) = good(&server);
    fixture.run(&server).assert_ready();
    server.reset_count();
    let run = fixture.run(&server);
    run.assert_ready();
    assert_eq!(run.field("check"), "valid");
    assert_eq!(server.requests(), 0, "a valid receipt must not GET");
}

#[test]
fn reason_only_change_keeps_key_and_makes_zero_gets() {
    let server = Server::start();
    let (fixture, archive) = good(&server);
    fixture.run(&server).assert_ready();
    fixture.set_descriptor(&descriptor(
        &[FileSpec::for_bytes(URL, &archive)],
        "new words",
        "",
    ));
    assert_eq!(w4_state(&fixture), W4::Ready);
    server.reset_count();
    fixture.run(&server).assert_ready();
    assert_eq!(server.requests(), 0);
}

#[test]
fn changed_fetch_block_is_receipt_stale_and_refetches() {
    let server = Server::start();
    let (fixture, _) = good(&server);
    fixture.run(&server).assert_ready();
    let v2 = good_archive("v2");
    server.route(
        "fixture.test",
        "/rel/b.tar.gz",
        Reply::Body {
            status: 200,
            body: v2.clone(),
        },
    );
    fixture.set_descriptor(&descriptor(
        &[FileSpec::for_bytes(
            "https://fixture.test/rel/b.tar.gz",
            &v2,
        )],
        "r",
        "",
    ));
    assert_eq!(w4_state(&fixture), W4::NeedsSetup("receipt_stale"));
    server.reset_count();
    let run = fixture.run(&server);
    run.assert_ready();
    assert_eq!(run.field("check"), "receipt_stale");
    assert_eq!(server.requests(), 1);
    assert_ready_with(&fixture, &good_payload("v2"));
}

#[test]
fn repair_is_an_ordinary_run_that_refetches_only_when_the_full_check_fails() {
    let server = Server::start();
    let (fixture, _) = good(&server);
    fixture.run(&server).assert_ready();
    // Same size, different bytes: W4's stat-only check still says Ready ...
    fs::write(fixture.state.join("assets/data/a.txt"), b"ALPHA v1\n").unwrap();
    assert_eq!(w4_state(&fixture), W4::Ready);
    // ... but the full SHA256 re-check in pf-app-fetch does not, so the run re-fetches.
    server.reset_count();
    let run = fixture.run(&server);
    run.assert_ready();
    assert_eq!(run.field("check"), "state_invalid");
    assert_eq!(server.requests(), 1);
    assert_ready_with(&fixture, &good_payload("v1"));
    // An extra file is also a failed full check.
    fs::write(fixture.state.join("assets/extra"), b"").unwrap();
    server.reset_count();
    fixture.run(&server).assert_ready();
    assert_eq!(server.requests(), 1);
    assert_ready_with(&fixture, &good_payload("v1"));
}

#[test]
fn sha_mismatch_is_digest_mismatch_and_leaves_needs_setup() {
    let server = Server::start();
    let archive = good_archive("v1");
    let mut spec = FileSpec::for_bytes(URL, &archive);
    spec.sha256 = "0".repeat(64);
    server.route(
        "fixture.test",
        PATH,
        Reply::Body {
            status: 200,
            body: archive,
        },
    );
    let fixture = Fixture::new(&descriptor(&[spec], "r", ""));
    let run = fixture.run(&server);
    run.assert_failed("digest_mismatch");
    assert_eq!(run.field("class"), "source_mismatch");
    assert_eq!(run.field("retryable"), false);
    assert_eq!(w4_state(&fixture), W4::NeedsSetup("absent"));
    assert!(fixture.payload().is_empty());
    fixture.assert_no_leftovers();
}

#[test]
fn complete_short_body_is_size_mismatch() {
    let server = Server::start();
    let archive = good_archive("v1");
    let mut spec = FileSpec::for_bytes(URL, &archive);
    spec.size += 10;
    server.route(
        "fixture.test",
        PATH,
        Reply::Body {
            status: 200,
            body: archive,
        },
    );
    let fixture = Fixture::new(&descriptor(&[spec], "r", ""));
    fixture.run(&server).assert_failed("size_mismatch");
}

#[test]
fn unlengthed_short_body_is_size_mismatch() {
    let server = Server::start();
    let archive = good_archive("v1");
    let mut spec = FileSpec::for_bytes(URL, &archive);
    spec.size += 10;
    server.route("fixture.test", PATH, Reply::NoLength { body: archive });
    let fixture = Fixture::new(&descriptor(&[spec], "r", ""));
    fixture.run(&server).assert_failed("size_mismatch");
}

#[test]
fn content_length_over_size_is_download_too_large() {
    let server = Server::start();
    let archive = good_archive("v1");
    let mut spec = FileSpec::for_bytes(URL, &archive);
    spec.size -= 1;
    server.route(
        "fixture.test",
        PATH,
        Reply::Body {
            status: 200,
            body: archive,
        },
    );
    let fixture = Fixture::new(&descriptor(&[spec], "r", ""));
    fixture.run(&server).assert_failed("download_too_large");
}

#[test]
fn unlengthed_body_over_size_is_download_too_large() {
    let server = Server::start();
    let archive = good_archive("v1");
    let mut spec = FileSpec::for_bytes(URL, &archive);
    spec.size -= 1;
    server.route("fixture.test", PATH, Reply::NoLength { body: archive });
    let fixture = Fixture::new(&descriptor(&[spec], "r", ""));
    fixture.run(&server).assert_failed("download_too_large");
}

#[test]
fn early_close_is_download_truncated() {
    let server = Server::start();
    let archive = good_archive("v1");
    let spec = FileSpec::for_bytes(URL, &archive);
    server.route(
        "fixture.test",
        PATH,
        Reply::Truncate {
            content_length: archive.len() as u64,
            body: archive[..archive.len() / 2].to_vec(),
        },
    );
    let fixture = Fixture::new(&descriptor(&[spec], "r", ""));
    let run = fixture.run(&server);
    run.assert_failed("download_truncated");
    assert_eq!(run.field("class"), "network");
    assert_eq!(run.field("retryable"), true);
}

#[test]
fn nothing_listening_is_offline() {
    let server = Server::start();
    let archive = good_archive("v1");
    let fixture = Fixture::new(&descriptor(&[FileSpec::for_bytes(URL, &archive)], "r", ""));
    let port = closed_port();
    let run = fixture.run_with(&server, |c| {
        c.env(
            "PF_APP_FETCH_TEST_RESOLVE",
            format!("fixture.test=127.0.0.1:{port}"),
        );
    });
    run.assert_failed("offline");
    assert_eq!(run.field("class"), "network");
}

#[test]
fn unresolvable_host_is_offline() {
    let server = Server::start();
    let archive = good_archive("v1");
    let fixture = Fixture::new(&descriptor(
        &[FileSpec::for_bytes(
            "https://nowhere.invalid/a.tar.gz",
            &archive,
        )],
        "r",
        "",
    ));
    fixture.run(&server).assert_failed("offline");
}

#[test]
fn untrusted_certificate_is_offline() {
    let server = Server::start();
    let (fixture, _) = good(&server);
    let leaf = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tls/leaf.der");
    let run = fixture.run_with(&server, |c| {
        c.env("PF_APP_FETCH_TEST_CA", leaf);
    });
    run.assert_failed("offline");
}

#[test]
fn http_statuses_map_to_w0_codes() {
    let cases: &[(u16, &str)] = &[
        (404, "http_refused"),
        (410, "http_refused"),
        (403, "http_refused"),
        (503, "http_error"),
        (500, "http_error"),
        (429, "http_error"),
        (408, "http_error"),
        (204, "http_error"),
        (206, "http_error"),
        (304, "http_error"),
        (299, "http_error"),
    ];
    for &(status, code) in cases {
        let server = Server::start();
        let archive = good_archive("v1");
        server.route(
            "fixture.test",
            PATH,
            Reply::Body {
                status,
                body: b"x".to_vec(),
            },
        );
        let fixture = Fixture::new(&descriptor(&[FileSpec::for_bytes(URL, &archive)], "r", ""));
        let run = fixture.run(&server);
        assert_eq!(
            run.reason().as_deref(),
            Some(code),
            "status {status}: {}",
            run.stderr()
        );
        run.assert_failed(code);
    }
    let server = Server::start();
    server.route(
        "fixture.test",
        PATH,
        Reply::Body {
            status: 404,
            body: vec![],
        },
    );
    let fixture = Fixture::new(&descriptor(&[FileSpec::for_bytes(URL, b"x")], "r", ""));
    let run = fixture.run(&server);
    assert_eq!(run.field("class"), "source_gone");
    assert_eq!(run.field("retryable"), false);
}

#[test]
fn in_scope_redirect_hops_are_followed() {
    let server = Server::start();
    let archive = good_archive("v1");
    server.route(
        "fixture.test",
        PATH,
        Reply::Redirect {
            status: 302,
            location: Some("https://cdn.fixture.test/obj/1?sig=a".into()),
        },
    );
    server.route(
        "cdn.fixture.test",
        "/obj/1?sig=a",
        Reply::Redirect {
            status: 307,
            location: Some("/obj/2".into()),
        },
    );
    server.route(
        "cdn.fixture.test",
        "/obj/2",
        Reply::Body {
            status: 200,
            body: archive.clone(),
        },
    );
    let fixture = Fixture::new(&descriptor(&[FileSpec::for_bytes(URL, &archive)], "r", ""));
    fixture.run(&server).assert_ready();
    assert_eq!(server.requests(), 3);
    assert_ready_with(&fixture, &good_payload("v1"));
}

#[test]
fn off_scope_redirect_is_refused_before_connecting() {
    let server = Server::start();
    let archive = good_archive("v1");
    server.route(
        "fixture.test",
        PATH,
        Reply::Redirect {
            status: 302,
            location: Some("https://off.fixture.test/x".into()),
        },
    );
    server.route(
        "off.fixture.test",
        "/x",
        Reply::Body {
            status: 200,
            body: archive.clone(),
        },
    );
    let fixture = Fixture::new(&descriptor(&[FileSpec::for_bytes(URL, &archive)], "r", ""));
    let run = fixture.run(&server);
    run.assert_failed("redirect_refused");
    assert_eq!(run.field("class"), "source_moved");
    assert_eq!(run.field("retryable"), false);
    assert!(
        server
            .log()
            .iter()
            .all(|(host, _)| host != "off.fixture.test"),
        "the off-scope hop must never connect: {:?}",
        server.log()
    );
}

type Routes = Vec<(&'static str, &'static str, Reply)>;

#[test]
fn every_bad_redirect_shape_is_redirect_refused() {
    let shapes: Vec<(&str, Routes)> = vec![
        (
            "http downgrade",
            vec![(
                "fixture.test",
                PATH,
                Reply::Redirect {
                    status: 302,
                    location: Some("http://cdn.fixture.test/x".into()),
                },
            )],
        ),
        (
            "missing location",
            vec![(
                "fixture.test",
                PATH,
                Reply::Redirect {
                    status: 302,
                    location: None,
                },
            )],
        ),
        (
            "300",
            vec![(
                "fixture.test",
                PATH,
                Reply::Redirect {
                    status: 300,
                    location: Some("/other".into()),
                },
            )],
        ),
        (
            "305",
            vec![(
                "fixture.test",
                PATH,
                Reply::Redirect {
                    status: 305,
                    location: Some("/other".into()),
                },
            )],
        ),
        (
            "userinfo",
            vec![(
                "fixture.test",
                PATH,
                Reply::Redirect {
                    status: 302,
                    location: Some("https://u:p@cdn.fixture.test/x".into()),
                },
            )],
        ),
        (
            "ip literal",
            vec![(
                "fixture.test",
                PATH,
                Reply::Redirect {
                    status: 302,
                    location: Some("https://127.0.0.1/x".into()),
                },
            )],
        ),
        (
            "other port",
            vec![(
                "fixture.test",
                PATH,
                Reply::Redirect {
                    status: 302,
                    location: Some("https://cdn.fixture.test:8443/x".into()),
                },
            )],
        ),
        (
            "loop",
            vec![
                (
                    "fixture.test",
                    PATH,
                    Reply::Redirect {
                        status: 302,
                        location: Some("/loop".into()),
                    },
                ),
                (
                    "fixture.test",
                    "/loop",
                    Reply::Redirect {
                        status: 302,
                        location: Some(PATH.into()),
                    },
                ),
            ],
        ),
        (
            "too many hops",
            vec![
                (
                    "fixture.test",
                    PATH,
                    Reply::Redirect {
                        status: 301,
                        location: Some("/h1".into()),
                    },
                ),
                (
                    "fixture.test",
                    "/h1",
                    Reply::Redirect {
                        status: 302,
                        location: Some("/h2".into()),
                    },
                ),
                (
                    "fixture.test",
                    "/h2",
                    Reply::Redirect {
                        status: 303,
                        location: Some("/h3".into()),
                    },
                ),
                (
                    "fixture.test",
                    "/h3",
                    Reply::Redirect {
                        status: 307,
                        location: Some("/h4".into()),
                    },
                ),
                (
                    "fixture.test",
                    "/h4",
                    Reply::Redirect {
                        status: 308,
                        location: Some("/h5".into()),
                    },
                ),
                (
                    "fixture.test",
                    "/h5",
                    Reply::Redirect {
                        status: 302,
                        location: Some("/h6".into()),
                    },
                ),
                (
                    "fixture.test",
                    "/h6",
                    Reply::Body {
                        status: 200,
                        body: b"x".to_vec(),
                    },
                ),
            ],
        ),
    ];
    for (name, routes) in shapes {
        let server = Server::start();
        for (host, path, reply) in routes {
            server.route(host, path, reply);
        }
        let fixture = Fixture::new(&descriptor(&[FileSpec::for_bytes(URL, b"x")], "r", ""));
        let run = fixture.run(&server);
        assert_eq!(
            run.reason().as_deref(),
            Some("redirect_refused"),
            "{name}: {}",
            run.stderr()
        );
        run.assert_failed("redirect_refused");
    }
}

#[test]
fn five_hops_is_the_limit_not_an_error() {
    let server = Server::start();
    let archive = good_archive("v1");
    server.route(
        "fixture.test",
        PATH,
        Reply::Redirect {
            status: 301,
            location: Some("/h1".into()),
        },
    );
    for hop in 1..5 {
        server.route(
            "fixture.test",
            &format!("/h{hop}"),
            Reply::Redirect {
                status: 302,
                location: Some(format!("/h{}", hop + 1)),
            },
        );
    }
    server.route(
        "fixture.test",
        "/h5",
        Reply::Body {
            status: 200,
            body: archive.clone(),
        },
    );
    let fixture = Fixture::new(&descriptor(&[FileSpec::for_bytes(URL, &archive)], "r", ""));
    fixture.run(&server).assert_ready();
}

fn unsafe_case(name: &str, archive: Vec<u8>, code: &str) {
    let server = Server::start();
    server.route(
        "fixture.test",
        PATH,
        Reply::Body {
            status: 200,
            body: archive.clone(),
        },
    );
    let mut spec = FileSpec::for_bytes(URL, &archive);
    spec.include = None;
    spec.strip = None;
    let fixture = Fixture::new(&descriptor(&[spec], "r", ""));
    let run = fixture.run(&server);
    assert_eq!(
        run.reason().as_deref(),
        Some(code),
        "{name}: {}",
        run.stderr()
    );
    run.assert_failed(code);
    assert_eq!(w4_state(&fixture), W4::NeedsSetup("absent"), "{name}");
    assert!(fixture.payload().is_empty(), "{name} left payload");
    fixture.assert_no_leftovers();
    assert!(!fixture.root.join("escape").exists(), "{name} escaped");
}

#[test]
fn unsafe_members_are_archive_member_unsafe() {
    let ok = || Tar::new().file("a.txt", b"a");
    unsafe_case(
        "dotdot",
        ok().file("../escape", b"x").gz(),
        "archive_member_unsafe",
    );
    unsafe_case(
        "inner dotdot",
        ok().file("d/../../escape", b"x").gz(),
        "archive_member_unsafe",
    );
    unsafe_case(
        "absolute",
        ok().file("/tmp/escape", b"x").gz(),
        "archive_member_unsafe",
    );
    unsafe_case(
        "symlink",
        ok().symlink("link", "/etc/passwd").gz(),
        "archive_member_unsafe",
    );
    unsafe_case(
        "relative symlink",
        ok().symlink("link", "a.txt").gz(),
        "archive_member_unsafe",
    );
    unsafe_case(
        "hardlink",
        ok().hardlink("hard", "a.txt").gz(),
        "archive_member_unsafe",
    );
    unsafe_case(
        "char device",
        ok().special("tty", b'3').gz(),
        "archive_member_unsafe",
    );
    unsafe_case(
        "block device",
        ok().special("sda", b'4').gz(),
        "archive_member_unsafe",
    );
    unsafe_case(
        "fifo",
        ok().special("pipe", b'6').gz(),
        "archive_member_unsafe",
    );
    unsafe_case(
        "duplicate",
        ok().file("a.txt", b"b").gz(),
        "archive_member_unsafe",
    );
    unsafe_case(
        "file then dir",
        ok().dir("a.txt/").gz(),
        "archive_member_unsafe",
    );
}

#[test]
fn unsafe_members_outside_include_are_never_extracted_and_do_not_fail() {
    // The good archive carries a symlink outside `Rel/data/` (release tarballs do).
    let server = Server::start();
    let (fixture, _) = good(&server);
    fixture.run(&server).assert_ready();
}

#[test]
fn archive_shape_failures_are_archive_invalid() {
    unsafe_case(
        "not gzip",
        b"definitely not gzip".to_vec(),
        "archive_invalid",
    );
    unsafe_case("empty tar", Tar::new().gz(), "archive_invalid");
    let server = Server::start();
    let archive = good_archive("v1");
    server.route(
        "fixture.test",
        PATH,
        Reply::Body {
            status: 200,
            body: archive.clone(),
        },
    );
    let mut spec = FileSpec::for_bytes(URL, &archive);
    spec.include = Some(vec!["Rel/nothing/".into()]);
    let fixture = Fixture::new(&descriptor(&[spec], "r", ""));
    fixture.run(&server).assert_failed("archive_invalid");
}

#[test]
fn descriptor_problems_are_descriptor_invalid_before_any_get() {
    let archive = good_archive("v1");
    let two = |dest_b: &str| {
        let a = FileSpec::for_bytes(URL, &archive);
        let mut b = FileSpec::for_bytes("https://fixture.test/rel/b.tar.gz", &archive);
        b.dest = dest_b.into();
        descriptor(&[a, b], "r", "")
    };
    let with = |edit: &dyn Fn(&mut FileSpec)| {
        let mut spec = FileSpec::for_bytes(URL, &archive);
        edit(&mut spec);
        descriptor(&[spec], "r", "")
    };
    let cases: Vec<(&str, String)> = vec![
        ("same dest twice", two("assets")),
        (
            "http url",
            with(&|s| s.url = "http://fixture.test/a.tar.gz".into()),
        ),
        (
            "userinfo url",
            with(&|s| s.url = "https://u@fixture.test/a.tar.gz".into()),
        ),
        (
            "ip literal url",
            with(&|s| s.url = "https://127.0.0.1/a.tar.gz".into()),
        ),
        ("zip format", with(&|s| s.format = "zip".into())),
        ("reserved dest", with(&|s| s.dest = ".pf-fetch".into())),
        ("unparseable", "schema_version = 3\n[app\n".to_string()),
        (
            "executable map",
            descriptor(&[FileSpec::for_bytes(URL, &archive)], "r", "").replace(
                "dest = \"assets\"\n",
                "dest = \"assets\"\nexecutable = { \"x\" = \"y\" }\n",
            ),
        ),
        (
            "disabled",
            descriptor(&[FileSpec::for_bytes(URL, &archive)], "r", "")
                .replace("enabled = true", "enabled = false"),
        ),
    ];
    for (name, text) in cases {
        let server = Server::start();
        server.route(
            "fixture.test",
            PATH,
            Reply::Body {
                status: 200,
                body: archive.clone(),
            },
        );
        let fixture = Fixture::new(&text);
        let run = fixture.run(&server);
        assert_eq!(
            run.reason().as_deref(),
            Some("descriptor_invalid"),
            "{name}: {}",
            run.stderr()
        );
        run.assert_failed("descriptor_invalid");
        assert_eq!(server.requests(), 0, "{name} must not GET");
    }
}

#[test]
fn insufficient_space_is_disk_full_before_the_get() {
    let server = Server::start();
    let (fixture, _) = good(&server);
    let run = fixture.run_with(&server, |c| {
        c.env("PF_APP_FETCH_TEST_FREE_BYTES", "1000");
    });
    run.assert_failed("disk_full");
    assert_eq!(run.field("class"), "storage");
    assert_eq!(server.requests(), 0);
}

#[test]
fn sigterm_mid_download_is_cancelled() {
    let server = Server::start();
    let archive = good_archive("v1");
    server.route(
        "fixture.test",
        PATH,
        Reply::Stall {
            content_length: archive.len() as u64,
            body: archive[..10].to_vec(),
            stall: Duration::from_secs(30),
        },
    );
    let fixture = Fixture::new(&descriptor(&[FileSpec::for_bytes(URL, &archive)], "r", ""));
    let child = fixture
        .command(&server)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while server.requests() == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(300));
    let sent = std::time::Instant::now();
    // SAFETY: plain kill(2) on our own child.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let output = child.wait_with_output().unwrap();
    // Review r1 P2-1: prompt even though the read is stalled for 30 s, well inside the
    // unit's TimeoutStopSec=10s.
    let latency = sent.elapsed();
    assert!(latency < Duration::from_secs(3), "cancel took {latency:?}");
    let run = Run {
        output,
        result: fixture.result(),
    };
    run.assert_failed("cancelled");
    assert_eq!(run.field("retryable"), true);
    fixture.assert_no_leftovers();
}

#[test]
fn version_reports_hooks_state() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_pf-app-fetch"))
        .arg("--version")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    // Test binaries are debug builds; the image ships release (test-hooks=off).
    assert!(text.contains("test-hooks=on"), "{text}");
}

#[test]
fn landlock_confined_run_installs_when_the_kernel_has_net_rules() {
    // The release binary always requires Landlock ABI >= 4. Hosts without it (some CI
    // sandboxes) run this best-effort; the a133 kernel is ABI 10.
    let server = Server::start();
    let (fixture, _) = good(&server);
    let mode = std::env::var("PF_APP_FETCH_CONFINE_TEST_MODE");
    let run = fixture.run_with(&server, |c| {
        // PF_APP_FETCH_CONFINE_TEST_MODE=required proves enforcement on a known host.
        c.env(
            "PF_APP_FETCH_TEST_CONFINE",
            mode.as_deref().unwrap_or("best-effort"),
        );
    });
    run.assert_ready();
    assert_ready_with(&fixture, &good_payload("v1"));
    if mode.as_deref() == Ok("required") {
        assert!(run.stderr().contains("landlock=abi"), "{}", run.stderr());
    }
}
