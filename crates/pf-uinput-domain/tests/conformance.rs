use pf_uinput_domain::*;
use sha2::{Digest, Sha256};
use std::cell::Cell;

const FIXTURE: &[u8] = include_bytes!("../../../tests/fixtures/uinput-domain-v1.tsv");
const FIXTURE_SHA256: &str = "538fce952f5de6cb901c46c1919aedc44dcd436e753f7786c1a6e5fe21ad2fb9";

fn check(value: &str) -> Check {
    match value {
        "no" => Check::No,
        "yes" => Check::Yes,
        "error" => Check::Error,
        _ => panic!("bad check {value}"),
    }
}
fn dt(value: &str) -> DtState {
    match value {
        "missing" => DtState::Missing,
        "ok" => DtState::Ok,
        "error" => DtState::Error,
        _ => panic!("bad dt {value}"),
    }
}
fn marker(value: &str) -> MarkerState {
    match value {
        "missing" => MarkerState::Missing,
        "valid" => MarkerState::Valid,
        "invalid" => MarkerState::Invalid,
        "error" => MarkerState::Error,
        _ => panic!("bad marker {value}"),
    }
}
fn logind(value: &str) -> LogindState {
    match value {
        "not-probed" => LogindState::NotProbed,
        "no-seat" | "mir-seatless" => LogindState::NoSeat,
        "wayland-seat" | "x11-seat" | "tty-seat" | "mir-seat" | "unspecified-seat"
        | "unknown-seat" | "empty-type-seat" => LogindState::Seat,
        "unreadable" => LogindState::Unreadable,
        _ => panic!("bad logind {value}"),
    }
}

#[test]
fn shared_fixture_and_mocked_opener_conform() {
    assert_eq!(format!("{:x}", Sha256::digest(FIXTURE)), FIXTURE_SHA256);
    let text = std::str::from_utf8(FIXTURE).unwrap();
    for line in text.lines().skip(1) {
        let f: Vec<_> = line.split('\t').collect();
        assert_eq!(f.len(), 11, "{line}");
        let probe = Probe {
            container: check(f[1]),
            vm: check(f[2]),
            dt: dt(f[3]),
            compatibles: if f[4] == "-" {
                vec![]
            } else {
                f[4].split(';').map(str::to_owned).collect()
            },
            marker: marker(f[5]),
            logind: logind(f[6]),
        };
        let got = evaluate(&probe);
        assert_eq!(got.allowed, f[7] == "allow", "{}", f[0]);
        assert_eq!(got.reason, f[8], "{}", f[0]);
        assert_eq!(got.domain.as_str(), f[9], "{}", f[0]);
        assert_eq!(got.probe.unwrap_or("-"), f[10], "{}", f[0]);
        let calls = Cell::new(0);
        let opened = guarded_open(&probe, || {
            calls.set(calls.get() + 1);
            Ok(())
        });
        assert_eq!(opened.is_ok(), got.allowed, "{}", f[0]);
        assert_eq!(calls.get(), usize::from(got.allowed), "{}", f[0]);
        if let Err(error) = opened {
            assert!(error.to_string().contains("reason=unsafe_uinput_domain"));
        }
    }
}
