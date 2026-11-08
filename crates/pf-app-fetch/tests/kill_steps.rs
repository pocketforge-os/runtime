//! SIGKILL at each protocol step (tsp-ziac.4.3.5 acceptance; §4.6 item 3 as amended by W3:
//! sweep, check, invalidate, move-aside + delete, stage, rename, commit).
//!
//! The debug-build hook `PF_APP_FETCH_TEST_STOP_AT=<step>` makes the binary write
//! `PF_APP_FETCH_TEST_STOP_MARKER` and `raise(SIGSTOP)`. The harness then SIGKILLs it,
//! asserts what the launcher (W4) would see — NeedsSetup or the untouched old state, never
//! a mixed Ready — and reruns, which must complete with the new payload.

mod common;

use common::*;
use std::fs;
use std::process::Stdio;
use std::time::{Duration, Instant};

const STEPS: &[&str] = &[
    "after-invalidate",
    "after-move-aside",
    "after-stage",
    "after-rename",
    "after-commit",
];

fn kill_at(fixture: &Fixture, server: &Server, step: &str) {
    let marker = fixture.root.join(format!("stopped-{step}"));
    let mut child = fixture
        .command(server)
        .env("PF_APP_FETCH_TEST_STOP_AT", step)
        .env("PF_APP_FETCH_TEST_STOP_MARKER", &marker)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pf-app-fetch");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !marker.exists() {
        if let Ok(Some(status)) = child.try_wait() {
            let mut err = String::new();
            use std::io::Read;
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut err)
                .unwrap();
            panic!("{step}: exited {status} before the stop point: {err}");
        }
        assert!(Instant::now() < deadline, "{step}: never reached");
        std::thread::sleep(Duration::from_millis(10));
    }
    // SAFETY: kill(2) on our own child.
    unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
    let status = child.wait().unwrap();
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(libc::SIGKILL),
        "{step}: killed"
    );
}

/// Start from an old Ready (v1), change the block to v2, kill the replacing run at `step`.
#[test]
fn replace_killed_at_each_step_is_never_a_mixed_ready_and_reruns_to_ready() {
    for &step in STEPS {
        let server = Server::start();
        let v1 = good_archive("v1");
        let v2 = good_archive("v2");
        server.route(
            "fixture.test",
            "/v1.tar.gz",
            Reply::Body {
                status: 200,
                body: v1.clone(),
            },
        );
        server.route(
            "fixture.test",
            "/v2.tar.gz",
            Reply::Body {
                status: 200,
                body: v2.clone(),
            },
        );
        let d1 = descriptor(
            &[FileSpec::for_bytes("https://fixture.test/v1.tar.gz", &v1)],
            "r",
            "",
        );
        let d2 = descriptor(
            &[FileSpec::for_bytes("https://fixture.test/v2.tar.gz", &v2)],
            "r",
            "",
        );
        let fixture = Fixture::new(&d1);
        fixture.run(&server).assert_ready();
        assert_ready_with(&fixture, &good_payload("v1"));

        fixture.set_descriptor(&d2);
        kill_at(&fixture, &server, step);
        let state = w4_state(&fixture);
        match state {
            W4::Ready => {
                // Only a kill after the commit may be Ready, and then it is the NEW payload.
                assert_eq!(step, "after-commit", "{step}: Ready before the commit");
                assert_ready_with(&fixture, &good_payload("v2"));
            }
            W4::NeedsSetup(_) => {}
        }
        // Under the OLD descriptor the old receipt never validates a mixed tree either.
        fixture.set_descriptor(&d1);
        if w4_state(&fixture) == W4::Ready {
            assert_ready_with(&fixture, &good_payload("v1"));
        }
        fixture.set_descriptor(&d2);

        let run = fixture.run(&server);
        run.assert_ready();
        assert_ready_with(&fixture, &good_payload("v2"));
        fixture.assert_no_leftovers();
    }
}

/// A first install killed at each step: NeedsSetup until the commit, then a rerun finishes.
#[test]
fn first_install_killed_at_each_step_reruns_to_ready() {
    for &step in STEPS {
        if step == "after-move-aside" {
            continue; // nothing to move aside on a first install; covered above
        }
        let server = Server::start();
        let v1 = good_archive("v1");
        server.route(
            "fixture.test",
            "/v1.tar.gz",
            Reply::Body {
                status: 200,
                body: v1.clone(),
            },
        );
        let fixture = Fixture::new(&descriptor(
            &[FileSpec::for_bytes("https://fixture.test/v1.tar.gz", &v1)],
            "r",
            "",
        ));
        kill_at(&fixture, &server, step);
        if step != "after-commit" {
            assert_ne!(w4_state(&fixture), W4::Ready, "{step}");
        }
        fixture.run(&server).assert_ready();
        assert_ready_with(&fixture, &good_payload("v1"));
        fixture.assert_no_leftovers();
    }
}

/// The step-1 sweep removes `.staging.*` and `.old.*` left by a killed run.
#[test]
fn sweep_removes_leftovers_of_a_killed_run() {
    let server = Server::start();
    let v1 = good_archive("v1");
    server.route(
        "fixture.test",
        "/v1.tar.gz",
        Reply::Body {
            status: 200,
            body: v1.clone(),
        },
    );
    let fixture = Fixture::new(&descriptor(
        &[FileSpec::for_bytes("https://fixture.test/v1.tar.gz", &v1)],
        "r",
        "",
    ));
    kill_at(&fixture, &server, "after-stage");
    let names: Vec<String> = fs::read_dir(fixture.state.join(".pf-fetch"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n.starts_with(".staging.")),
        "{names:?}"
    );
    fixture.run(&server).assert_ready();
    fixture.assert_no_leftovers();
}
