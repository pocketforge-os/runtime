use pf_scene::{Bounds, Node, NodeId, Role};
use pf_theme::{flagship, load, load_or_flagship, Base, LoadError, ResolveError};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
fn vendor() -> PathBuf {
    crate_dir().join("vendor/package")
}
fn scratch(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("pf-theme-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}
fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for e in fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let to = dst.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &to)
        } else {
            fs::copy(e.path(), to).unwrap();
        }
    }
}

#[test]
fn flagship_passes_all_load_gates() {
    let theme = load(vendor()).expect("vendored flagship must validate");
    assert_eq!(theme.manifest().id, "quiet-console");
}

#[test]
fn resolution_scene_motion_and_fallback_are_typed() {
    let theme = flagship();
    assert_eq!(
        theme.resolve(Base::Dark, "--color-text-primary").unwrap(),
        "#f4efe6"
    );
    let node = Node::new(
        NodeId::new("label").unwrap(),
        Role::Text,
        "Label",
        Bounds::new(0., 0., 1., 1.),
        "--color-text-primary",
    );
    assert_eq!(theme.resolve_node(Base::Light, &node).unwrap(), "#26221a");
    assert_eq!(
        theme.resolve_motion("launch", false).unwrap().duration_ms,
        420
    );
    assert_eq!(theme.resolve_motion("launch", true).unwrap().duration_ms, 0);
    assert!(matches!(
        theme.resolve(Base::Dark, "--not-a-token"),
        Err(ResolveError::UnknownToken(_))
    ));

    let missing = scratch("fallback").join("missing");
    let (fallback, error) = load_or_flagship(&missing);
    assert!(error.is_some());
    assert_eq!(fallback.manifest().id, "quiet-console");
}

#[test]
fn broken_package_reports_a_specific_gate() {
    let root = scratch("broken");
    copy_tree(&vendor(), &root);
    let p = root.join("tokens.json");
    let mut json: Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
    json["bases"]["dark"]
        .as_object_mut()
        .unwrap()
        .remove("--color-text-primary");
    fs::write(&p, serde_json::to_vec_pretty(&json).unwrap()).unwrap();
    assert!(matches!(
        load(&root),
        Err(LoadError::MissingToken {
            base: Some(Base::Dark),
            ..
        })
    ));
}

#[cfg(unix)]
#[test]
fn malicious_paths_symlinks_and_svg_handlers_are_rejected() {
    use std::os::unix::fs::symlink;
    let root = scratch("symlink");
    copy_tree(&vendor(), &root);
    symlink("/etc/passwd", root.join("escape")).unwrap();
    assert!(matches!(load(&root), Err(LoadError::Symlink(_))));

    let root = scratch("escape");
    copy_tree(&vendor(), &root);
    let p = root.join("manifest.json");
    let mut json: Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
    json["assets"].as_array_mut().unwrap()[0]["path"] = Value::String("../escape.svg".into());
    fs::write(&p, serde_json::to_vec_pretty(&json).unwrap()).unwrap();
    assert!(matches!(load(&root), Err(LoadError::UnsafeAssetPath(_))));

    let root = scratch("onclick");
    copy_tree(&vendor(), &root);
    let asset = root.join("motifs/steps.svg");
    let source = fs::read_to_string(&asset)
        .unwrap()
        .replace("<svg ", "<svg onclick=\"steal()\" ");
    fs::write(&asset, &source).unwrap();
    let hash = std::process::Command::new("sha256sum")
        .arg(&asset)
        .output()
        .unwrap();
    let hash = String::from_utf8(hash.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let p = root.join("manifest.json");
    let mut json: Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
    json["assets"].as_array_mut().unwrap()[0]["sha256"] = Value::String(hash);
    fs::write(&p, serde_json::to_vec_pretty(&json).unwrap()).unwrap();
    assert!(matches!(
        load(&root),
        Err(LoadError::UnsafeAssetContent { .. })
    ));
}

#[test]
fn css_transform_is_stable() {
    let actual = flagship().to_css();
    let expected = fs::read_to_string(crate_dir().join("vendor/tokens.generated.css")).unwrap();
    assert_eq!(
        actual, expected,
        "update only alongside the design tokens source"
    );
}
