//! Review round 1 of runtime#188 (tsp-ziac.4.3.5): strip-removed members, a result.json
//! that can never look current when stale, and the two disk_full paths during extraction.

mod common;

use common::*;
use std::process::Stdio;
use std::time::{Duration, Instant};

const URL: &str = "https://fixture.test/rel/a.tar.gz";
const PATH: &str = "/rel/a.tar.gz";

fn serve(server: &Server, archive: &[u8]) {
    server.route(
        "fixture.test",
        PATH,
        Reply::Body {
            status: 200,
            body: archive.to_vec(),
        },
    );
}

/// P1-1: a member that strip_components removes entirely is skipped whatever its type,
/// like pf-fetch.py member_path() and GNU tar. MBHaxe's release tarball carries
/// `MBHaxe-Ultra-Linux/libhl.so*` symlinks at depth 2 under strip_components = 2.
#[test]
fn members_removed_entirely_by_strip_are_skipped_whatever_their_type() {
    let archive = Tar::new()
        .dir("Top/")
        .symlink("Top/libhl.so", "libhl.so.1")
        .symlink("Top/libhl.so.1", "libhl.so.1.0")
        .file("Top/run", b"#!/bin/sh\n")
        .special("Top/fifo", b'6')
        .dir("Top/data/")
        .file("Top/data/level.mis", b"level")
        .gz();
    let server = Server::start();
    serve(&server, &archive);
    let mut spec = FileSpec::for_bytes(URL, &archive);
    spec.strip = Some(2);
    spec.include = None;
    let fixture = Fixture::new(&descriptor(&[spec], "r", ""));
    fixture.run(&server).assert_ready();
    let mut want = std::collections::BTreeMap::new();
    want.insert("assets/level.mis".to_string(), b"level".to_vec());
    assert_ready_with(&fixture, &want);
}

/// The absolute and `..` refusals still come before the strip skip.
#[test]
fn absolute_and_dotdot_are_refused_even_when_strip_would_remove_them() {
    for name in ["/abs", "../up"] {
        let archive = Tar::new().file(name, b"x").file("Top/data/ok", b"ok").gz();
        let server = Server::start();
        serve(&server, &archive);
        let mut spec = FileSpec::for_bytes(URL, &archive);
        spec.strip = Some(2);
        spec.include = None;
        let fixture = Fixture::new(&descriptor(&[spec], "r", ""));
        fixture.run(&server).assert_failed("archive_member_unsafe");
    }
}

fn spawn_stopped(fixture: &Fixture, server: &Server, step: &str) -> std::process::Child {
    let marker = fixture.root.join(format!("stopped-{step}"));
    let mut child = fixture
        .command(server)
        .env("PF_APP_FETCH_TEST_STOP_AT", step)
        .env("PF_APP_FETCH_TEST_STOP_MARKER", &marker)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !marker.exists() {
        assert!(child.try_wait().unwrap().is_none(), "exited before {step}");
        assert!(Instant::now() < deadline, "never reached {step}");
        std::thread::sleep(Duration::from_millis(10));
    }
    child
}

/// P2-2: once a run holds the lock, result.json says `running` with that run's id, so a
/// previous verdict never looks current; a run that loses the lock leaves it untouched.
#[test]
fn result_json_is_running_while_a_run_holds_the_lock_and_losers_do_not_write_it() {
    let server = Server::start();
    let archive = good_archive("v1");
    serve(&server, &archive);
    let fixture = Fixture::new(&descriptor(&[FileSpec::for_bytes(URL, &archive)], "r", ""));
    let first = fixture.run(&server);
    first.assert_ready();
    let ready_id = first.field("run_id");

    // Force a re-fetch, and stop the new run while it holds the lock.
    std::fs::write(fixture.state.join("assets/data/a.txt"), b"ALPHA v1\n").unwrap();
    let mut child = spawn_stopped(&fixture, &server, "after-invalidate");
    let live = fixture.result().expect("result.json while running");
    assert_eq!(live["outcome"], "running", "{live}");
    assert_ne!(live["run_id"], ready_id);
    assert!(live["run_id"].as_str().is_some_and(|id| !id.is_empty()));
    assert_eq!(live["reason"], serde_json::Value::Null);

    // A concurrent run loses the flock: state_invalid on stderr, result.json untouched.
    let loser = fixture.run(&server);
    assert_eq!(loser.output.status.code(), Some(1));
    assert!(
        loser.stderr().contains("reason=state_invalid"),
        "{}",
        loser.stderr()
    );
    assert_eq!(
        fixture.result().unwrap(),
        live,
        "the lock loser rewrote result.json"
    );

    // SIGKILL leaves `running` (never a stale ready); a rerun completes.
    // SAFETY: kill(2) on our own child.
    unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
    child.wait().unwrap();
    assert_eq!(fixture.result().unwrap()["outcome"], "running");
    let rerun = fixture.run(&server);
    rerun.assert_ready();
    assert_ne!(rerun.field("run_id"), live["run_id"]);
    assert_ready_with(&fixture, &good_payload("v1"));
}

/// P2-3: the pre-extract precheck (gzip ISIZE + floor) is disk_full end to end, after
/// the download passed its own precheck.
#[test]
fn pre_extract_isize_precheck_is_disk_full() {
    // 24 MiB of zeros compresses to a few KiB: the download fits, the expansion does not.
    let big = vec![0u8; 24 << 20];
    let archive = Tar::new().file("Rel/data/zeros", &big).gz();
    let server = Server::start();
    serve(&server, &archive);
    let fixture = Fixture::new(&descriptor(&[FileSpec::for_bytes(URL, &archive)], "r", ""));
    let free = archive.len() as u64 + (32 << 20) + (1 << 20);
    let run = fixture.run_with(&server, |c| {
        c.env("PF_APP_FETCH_TEST_FREE_BYTES", free.to_string());
    });
    run.assert_failed("disk_full");
    assert_eq!(run.field("gets"), 1);
    assert!(run
        .field("detail")
        .as_str()
        .unwrap()
        .contains("before extracting"));
    assert_eq!(w4_state(&fixture), W4::NeedsSetup("absent"));
    fixture.assert_no_leftovers();
}

/// P2-3: ENOSPC while writing an extracted member maps to disk_full (debug hook that makes
/// the payload writer fail with ENOSPC after N bytes, through the real error path).
#[test]
fn enospc_mid_extract_is_disk_full() {
    let server = Server::start();
    let archive = good_archive("v1");
    serve(&server, &archive);
    let fixture = Fixture::new(&descriptor(&[FileSpec::for_bytes(URL, &archive)], "r", ""));
    let run = fixture.run_with(&server, |c| {
        c.env("PF_APP_FETCH_TEST_ENOSPC_AFTER", "100");
    });
    run.assert_failed("disk_full");
    assert!(run.field("detail").as_str().unwrap().contains("payload"));
    assert_eq!(w4_state(&fixture), W4::NeedsSetup("absent"));
    fixture.assert_no_leftovers();
}
