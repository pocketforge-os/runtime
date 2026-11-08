//! Cross-repo v3 fetch drift corpus.
//!
//! The corpus under `tests/fixtures/app-schema-corpus/` is stored byte-identically in
//! pocketforge-os/runtime and pocketforge-os/platform (regression/abi/fixtures/
//! app-schema-corpus). It pins the descriptors the image and MBHaxe ship today plus one
//! positive and one negative v3 case per fetch rule. `expected.json` records the digest
//! and the expected verdict for every descriptor, so neither repo can drift alone.

use pf_app_manifest::{parse_manifest, ManifestErrorKind};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/app-schema-corpus")
}

fn relative_files(dir: &Path, base: &Path, out: &mut Vec<(String, PathBuf)>) {
    for entry in fs::read_dir(dir).expect("read corpus directory") {
        let path = entry.expect("corpus entry").path();
        if path.is_dir() {
            relative_files(&path, base, out);
        } else {
            let relative = path
                .strip_prefix(base)
                .expect("corpus path below base")
                .to_string_lossy()
                .replace('\\', "/");
            out.push((relative, path));
        }
    }
}

fn described_files(root: &Path) -> Vec<(String, PathBuf)> {
    let mut files = Vec::new();
    relative_files(
        &root.join("descriptors"),
        &root.join("descriptors"),
        &mut files,
    );
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

fn corpus_digest(root: &Path) -> String {
    let mut digest = Sha256::new();
    for (relative, path) in described_files(root) {
        digest.update(relative.as_bytes());
        digest.update([0]);
        digest.update(fs::read(path).expect("read descriptor"));
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The stable message fragment a refusal must name for each reason class in the table,
/// so the semantic predicate cannot silently weaken.
fn refusal_marker(reason: &str) -> &'static str {
    match reason {
        "missing-size" => "requires size",
        "invalid-include" => "invalid fetch include",
        "invalid-dest" => "invalid fetch dest",
        "reserved-source" => "reserved; not supported yet",
        "v3-only-include" | "v3-only-source" => "schema_version 3",
        other => panic!("expected.json names unknown refusal reason {other}"),
    }
}

#[test]
fn corpus_digest_matches_expected_table() {
    let root = corpus_root();
    let table: Value = serde_json::from_str(
        &fs::read_to_string(root.join("expected.json")).expect("read expected.json"),
    )
    .expect("expected.json is valid JSON");
    assert_eq!(
        table["corpus_sha256"].as_str().expect("corpus_sha256"),
        corpus_digest(&root),
        "corpus bytes changed; update both repos byte-identically and the table together"
    );
}

#[test]
fn parser_matches_expected_results() {
    let root = corpus_root();
    let table: Value = serde_json::from_str(
        &fs::read_to_string(root.join("expected.json")).expect("read expected.json"),
    )
    .expect("expected.json is valid JSON");
    let cases = table["cases"].as_array().expect("cases array");
    assert_eq!(
        cases.len(),
        described_files(&root).len(),
        "unlisted corpus file"
    );

    for case in cases {
        let relative = case["path"].as_str().expect("case path");
        let source = fs::read_to_string(root.join("descriptors").join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        let result = parse_manifest(&source);
        match case["expect"].as_str().expect("case expect") {
            "accept" => {
                result.unwrap_or_else(|error| {
                    panic!("{relative}: expected accept, parser refused: {error}")
                });
            }
            "refuse" => {
                let error = match result {
                    Ok(manifest) => {
                        panic!("{relative}: expected refusal, parser accepted {manifest:?}")
                    }
                    Err(error) => error,
                };
                assert_eq!(
                    error.kind,
                    ManifestErrorKind::Invalid,
                    "{relative}: refusal must be a validation error, got {error}"
                );
                let marker = refusal_marker(case["reason"].as_str().expect("case reason"));
                assert!(
                    error.message.contains(marker),
                    "{relative}: refusal {error:?} must contain {marker:?}"
                );
            }
            other => panic!("{relative}: unknown expectation {other}"),
        }
    }
}
