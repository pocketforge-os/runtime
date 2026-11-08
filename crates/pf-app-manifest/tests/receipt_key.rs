//! The ONE shared `[fetch]` receipt key (infra-107.4 §4.6 item 3, tsp-ziac.4.3.5 / W3).
//!
//! pf-app-fetch (W3) writes this key into `.pf-fetch/receipt.json`; the launcher (W4)
//! recomputes it from the current descriptor to decide Ready vs `receipt_stale`. Both call
//! `pf_app_manifest::fetch_receipt_key`, and this committed vector fixes the encoding so
//! neither side can drift. The vector was computed independently of this crate with
//! `json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=False)` and SHA256.

use pf_app_manifest::{fetch_receipt_canonical_json, fetch_receipt_key, parse_manifest, Fetch};
use std::path::Path;

/// MBHaxe `pocketforge/app/app.toml@362fe951` (lines 63-86), stored byte-identically in the
/// cross-repo corpus.
fn mbhaxe_fetch() -> Fetch {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/app-schema-corpus/descriptors/mbhaxe/pocketforge/app/app.toml");
    let source = std::fs::read_to_string(path).expect("read MBHaxe descriptor");
    parse_manifest(&source)
        .expect("MBHaxe descriptor parses")
        .fetch
        .expect("MBHaxe has a [fetch] block")
}

const MBHAXE_CANONICAL: &str = concat!(
    r#"{"destination":"var","files":[{"dest":"data","format":"tar.gz","include":null,"#,
    r#""sha256":"515487e0f9eac03dd660ddedc856bee989c7921464aa96e3cb4f5407267573f2","#,
    r#""size":57876762,"strip_components":2,"#,
    r#""url":"https://github.com/RandomityGuy/MBHaxe/releases/download/1.3.2-mbu/MBHaxe-Ultra-Linux.tar.gz"}],"#,
    r#""source":"pinned"}"#
);
const MBHAXE_KEY: &str = "fe1f36f2a4e81b993c5229d7f23da379935b19783fb90081532c211a2a35e631";
/// The same block with W5's `include = ["MBHaxe-Ultra-Linux/data/"]`.
const MBHAXE_INCLUDE_KEY: &str = "b5e10a4d13136ad4423b3d28cf86c912f83d7d92b609d33a6e62e5af9a6b2afc";

#[test]
fn mbhaxe_vector_canonical_json_and_key() {
    let fetch = mbhaxe_fetch();
    assert_eq!(
        fetch_receipt_canonical_json(&fetch).as_deref(),
        Some(MBHAXE_CANONICAL)
    );
    assert_eq!(fetch_receipt_key(&fetch).as_deref(), Some(MBHAXE_KEY));
}

#[test]
fn mbhaxe_vector_with_include() {
    let mut fetch = mbhaxe_fetch();
    fetch.files.as_mut().unwrap()[0].include = Some(vec!["MBHaxe-Ultra-Linux/data/".into()]);
    assert_eq!(
        fetch_receipt_key(&fetch).as_deref(),
        Some(MBHAXE_INCLUDE_KEY)
    );
}

#[test]
fn canonical_json_agrees_with_serde_json_sorted_compact() {
    let fetch = mbhaxe_fetch();
    let ours = fetch_receipt_canonical_json(&fetch).expect("enabled block has a key");
    let value: serde_json::Value = serde_json::from_str(&ours).expect("valid JSON");
    // serde_json's default Map is a BTreeMap: to_string is sorted and compact.
    assert_eq!(serde_json::to_string(&value).unwrap(), ours);
}

#[test]
fn reason_text_and_enabled_irrelevant_fields_keep_the_key() {
    let base = mbhaxe_fetch();
    let mut reason = base.clone();
    reason.reason = Some("Completely different explanation text.".into());
    assert_eq!(fetch_receipt_key(&reason), fetch_receipt_key(&base));
}

#[test]
fn absent_source_and_strip_encode_as_their_defaults() {
    let base = mbhaxe_fetch();
    let mut explicit = base.clone();
    explicit.source = Some(pf_app_manifest::FetchSource::Pinned);
    assert_eq!(fetch_receipt_key(&explicit), fetch_receipt_key(&base));
    let mut no_strip = base.clone();
    no_strip.files.as_mut().unwrap()[0].strip_components = None;
    let mut zero_strip = base.clone();
    zero_strip.files.as_mut().unwrap()[0].strip_components = Some(0);
    assert_eq!(fetch_receipt_key(&no_strip), fetch_receipt_key(&zero_strip));
    assert_ne!(fetch_receipt_key(&no_strip), fetch_receipt_key(&base));
}

type Edit = Box<dyn Fn(&mut Fetch)>;

#[test]
fn every_keyed_field_changes_the_key() {
    let base = mbhaxe_fetch();
    let base_key = fetch_receipt_key(&base).expect("key");
    let edits: Vec<(&str, Edit)> = vec![
        (
            "url",
            Box::new(|f| f.files.as_mut().unwrap()[0].url.push('x')),
        ),
        (
            "sha256",
            Box::new(|f| f.files.as_mut().unwrap()[0].sha256 = "0".repeat(64)),
        ),
        (
            "size",
            Box::new(|f| f.files.as_mut().unwrap()[0].size = Some(1)),
        ),
        (
            "format",
            Box::new(|f| f.files.as_mut().unwrap()[0].format = Some("tar".into())),
        ),
        (
            "dest",
            Box::new(|f| f.files.as_mut().unwrap()[0].dest = Some("other".into())),
        ),
        (
            "strip",
            Box::new(|f| f.files.as_mut().unwrap()[0].strip_components = Some(1)),
        ),
        (
            "include",
            Box::new(|f| f.files.as_mut().unwrap()[0].include = Some(vec!["a/".into()])),
        ),
        ("destination", Box::new(|f| f.destination = None)),
        (
            "source",
            Box::new(|f| f.source = Some(pf_app_manifest::FetchSource::VendorSignedPointer)),
        ),
        (
            "second file",
            Box::new(|f| {
                let mut extra = f.files.as_ref().unwrap()[0].clone();
                extra.dest = Some("more".into());
                f.files.as_mut().unwrap().push(extra);
            }),
        ),
    ];
    for (name, edit) in edits {
        let mut changed = base.clone();
        edit(&mut changed);
        let key = fetch_receipt_key(&changed).expect("key");
        assert_ne!(key, base_key, "{name} must change the key");
    }
}

#[test]
fn file_order_is_significant() {
    let mut two = mbhaxe_fetch();
    let mut second = two.files.as_ref().unwrap()[0].clone();
    second.dest = Some("more".into());
    second.url = "https://example.org/b.tar.gz".into();
    two.files.as_mut().unwrap().push(second);
    let mut swapped = two.clone();
    swapped.files.as_mut().unwrap().reverse();
    assert_ne!(fetch_receipt_key(&two), fetch_receipt_key(&swapped));
}

#[test]
fn disabled_block_has_no_key() {
    let mut fetch = mbhaxe_fetch();
    fetch.enabled = false;
    assert_eq!(fetch_receipt_key(&fetch), None);
    assert_eq!(fetch_receipt_canonical_json(&fetch), None);
}

#[test]
fn strings_are_json_escaped() {
    let mut fetch = mbhaxe_fetch();
    fetch.files.as_mut().unwrap()[0].url = "https://h/\"q\\\u{1}\n\u{e9}".into();
    let json = fetch_receipt_canonical_json(&fetch).expect("key");
    let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    assert_eq!(value["files"][0]["url"], "https://h/\"q\\\u{1}\n\u{e9}");
    assert_eq!(serde_json::to_string(&value).unwrap(), json);
}
