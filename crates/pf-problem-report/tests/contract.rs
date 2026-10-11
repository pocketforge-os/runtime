use pf_problem_report::{decode, encode, RefusalReason, MAX_BYTES};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

fn platform_root() -> PathBuf {
    if let Some(path) = std::env::var_os("PF_PLATFORM_DIR") {
        return PathBuf::from(path);
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../platform")
}

fn fixture_root() -> PathBuf {
    let root = platform_root().join("schemas/problem-report-v1/fixtures");
    assert!(
        root.is_dir(),
        "problem-report fixtures missing at {}; set PF_PLATFORM_DIR to a real platform checkout",
        root.display()
    );
    root
}

fn fixtures(kind: &str) -> Vec<(PathBuf, Value)> {
    let dir = fixture_root().join(kind);
    let mut paths: Vec<_> = fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()))
        .map(|entry| entry.expect("fixture directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let bytes = fs::read(&path).expect("read fixture");
            let value = serde_json::from_slice(&bytes).expect("parse fixture JSON");
            (path, value)
        })
        .collect()
}

fn hex_bytes(text: &str) -> Vec<u8> {
    assert_eq!(text.len() % 2, 0);
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).expect("ASCII hex"), 16)
                .expect("valid fixture hex")
        })
        .collect()
}

fn tagged_fixture_to_json(value: &Value) -> Value {
    match value {
        Value::Object(object) if object.len() == 1 && object.contains_key("$bytes") => {
            object["$bytes"].clone()
        }
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), tagged_fixture_to_json(value)))
                .collect(),
        ),
        Value::Array(array) => Value::Array(array.iter().map(tagged_fixture_to_json).collect()),
        _ => value.clone(),
    }
}

#[test]
fn valid_platform_fixtures_round_trip_exact_canonical_bytes() {
    let fixtures = fixtures("valid");
    assert!(!fixtures.is_empty());
    for (path, fixture) in fixtures {
        let report = &fixture["report"];
        let bytes = hex_bytes(fixture["cbor_hex"].as_str().expect("cbor_hex string"));
        assert!(bytes.len() <= MAX_BYTES, "{}", path.display());
        assert_eq!(
            encode(report).expect("valid report"),
            bytes,
            "{}",
            path.display()
        );
        assert_eq!(
            decode(&bytes).expect("valid CBOR"),
            *report,
            "{}",
            path.display()
        );
    }
}

#[test]
fn invalid_platform_fixtures_return_the_named_reason() {
    let fixtures = fixtures("invalid");
    assert!(!fixtures.is_empty());
    for (path, fixture) in fixtures {
        let error = if let Some(hex) = fixture.get("cbor_hex").and_then(Value::as_str) {
            decode(&hex_bytes(hex)).expect_err("invalid wire fixture must be refused")
        } else {
            let report = tagged_fixture_to_json(&fixture["report"]);
            encode(&report).expect_err("invalid report fixture must be refused")
        };
        let expected = fixture["expected_reason"]
            .as_str()
            .expect("expected reason");
        assert_eq!(error.reason().as_str(), expected, "{}", path.display());
    }
}

#[test]
fn typed_unknown_frame_is_valid_but_question_mark_is_not() {
    let fixture = fixtures("valid")
        .into_iter()
        .find(|(path, _)| {
            path.file_name()
                .is_some_and(|name| name == "crash-sideloaded-unknown-frames.json")
        })
        .expect("typed unknown fixture")
        .1;
    let report = &fixture["report"];
    encode(report).expect("[null, null] is the typed unknown frame");

    let mut invalid = report.clone();
    invalid["frames"][0] = Value::String("?".into());
    assert_eq!(
        encode(&invalid)
            .expect_err("free-text unknown frame")
            .reason(),
        RefusalReason::Type
    );
}

#[derive(Clone)]
struct XorShift64(u64);

impl XorShift64 {
    fn next(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }

    fn below(&mut self, limit: usize) -> usize {
        self.next() as usize % limit
    }
}

fn generated_report(rng: &mut XorShift64, case: usize) -> Value {
    let kinds = ["crash", "hang", "oom", "component", "kernel"];
    let kind = kinds[case % kinds.len()];
    let del = format!("{:064x}", rng.next());
    let uptime = ["<1m", "1-10m", "10-60m", ">1h"][rng.below(4)];
    let mut report = json!({
        "v": 1,
        "kind": kind,
        "how": "SIGSEGV",
        "day": format!("20{:02}-{:02}-{:02}", 20 + rng.below(80), 1 + rng.below(12), 1 + rng.below(28)),
        "build": format!("{:012x}", rng.next() & 0xffffffffffff),
        "model": format!("model-{}", rng.below(1000)),
        "uptime": uptime,
        "del": del,
    });
    if kind == "kernel" {
        let how = ["oops", "panic", "watchdog"][rng.below(3)];
        report["how"] = json!(how);
        let count = if case == 4 { 32 } else { rng.below(33) };
        report["kframes"] = Value::Array(
            (0..count)
                .map(|index| {
                    json!([
                        format!("symbol_{index}"),
                        rng.next() & u32::MAX as u64,
                        128,
                        if index % 2 == 0 {
                            json!("module")
                        } else {
                            Value::Null
                        }
                    ])
                })
                .collect(),
        );
        return report;
    }

    let origin = if kind == "component" {
        "image"
    } else {
        ["image", "catalog", "sideloaded"][rng.below(3)]
    };
    report["app"] = match origin {
        "sideloaded" => json!({"origin": origin}),
        "catalog" => {
            json!({"id": "org.example.generated", "ver": format!("1.{}.0", rng.below(100)), "origin": origin})
        }
        _ => json!({"id": "os.pocketforge.generated", "ver": "1.0", "origin": origin}),
    };
    let session = ["<1m", "1-10m", "10-60m", ">1h"][rng.below(4)];
    report["session"] = json!(session);
    report["mem"] =
        json!({"pressure": rng.next() & 1 == 1, "oom": kind == "oom" || rng.next() & 1 == 1});
    let frame_count = if case < 5 { 48 } else { rng.below(49) };
    report["frames"] = Value::Array(
        (0..frame_count)
            .map(|index| {
                if (rng.next() + index as u64) % 7 == 0 {
                    json!([null, null])
                } else {
                    json!([
                        format!("{:016x}", rng.next()),
                        rng.next() & ((1_u64 << 48) - 1)
                    ])
                }
            })
            .collect(),
    );
    match kind {
        "crash" => {
            if case % 11 == 0 && origin == "image" {
                report["how"] = json!("SIGABRT");
                report["panic_at"] = json!("crates/pf-generated/src/lib.rs:12:3");
            }
        }
        "hang" => {
            let how = ["no-reply-menu", "stop-timeout"][rng.below(2)];
            report["how"] = json!(how);
            report["samples"] = if frame_count == 0 {
                json!([])
            } else if frame_count == 1 || rng.next() & 1 == 0 {
                json!([frame_count])
            } else {
                let first = 1 + rng.below(frame_count - 1);
                json!([first, frame_count - first])
            };
        }
        "oom" => report["how"] = json!("oom-kill"),
        "component" => {
            let how = ["SIGSEGV", "start-timeout", "stop-timeout", "no-reply-menu"][rng.below(4)];
            report["how"] = json!(how);
        }
        _ => unreachable!(),
    }
    report
}

#[test]
fn seeded_valid_reports_fit_and_round_trip() {
    let mut rng = XorShift64(0x5eed_2026_1011_0037);
    let mut largest = 0;
    for case in 0..400 {
        let report = generated_report(&mut rng, case);
        let bytes =
            encode(&report).unwrap_or_else(|error| panic!("case {case}: {error}; {report}"));
        largest = largest.max(bytes.len());
        assert!(
            bytes.len() <= MAX_BYTES,
            "case {case}: {} bytes",
            bytes.len()
        );
        assert_eq!(
            decode(&bytes).expect("generated report decodes"),
            report,
            "case {case}"
        );
        assert_eq!(
            encode(&decode(&bytes).unwrap()).unwrap(),
            bytes,
            "case {case}"
        );
    }
    assert!(
        largest > 1_000,
        "generator did not approach the size bound: {largest}"
    );
}

#[test]
fn unknown_vocabulary_and_free_text_mutations_are_refused() {
    let mut rng = XorShift64(0xabad_1dea_0000_0037);
    for case in 0..100 {
        let report = generated_report(&mut rng, case);
        encode(&report).expect("positive control");

        let mut unknown = report.clone();
        unknown["hostname"] = json!("pocket");
        assert_eq!(
            encode(&unknown).unwrap_err().reason(),
            RefusalReason::UnknownField
        );

        let mut vocabulary = report.clone();
        vocabulary["uptime"] = json!("1–10m");
        assert_eq!(
            encode(&vocabulary).unwrap_err().reason(),
            RefusalReason::Vocabulary
        );

        let mut free_text = report.clone();
        free_text["model"] = json!("user@example.com");
        assert_eq!(
            encode(&free_text).unwrap_err().reason(),
            RefusalReason::Grammar
        );
    }
}
