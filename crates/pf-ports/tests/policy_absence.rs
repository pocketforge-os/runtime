use std::fs;
use std::path::{Path, PathBuf};

fn source_files(root: &Path, output: &mut Vec<PathBuf>) {
    if !root.exists() {
        return;
    }
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            source_files(&path, output);
        } else {
            output.push(path);
        }
    }
}

#[test]
fn foundation_sources_contain_no_product_policy() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for relative in [
        "crates/pf-scene/src",
        "crates/pf-scene/fixtures",
        "crates/pf-ports/src",
        "crates/pf-ports/fixtures",
    ] {
        source_files(&workspace.join(relative), &mut files);
    }

    let prohibited = [
        concat!("Ho", "me"),
        concat!("Lib", "rary"),
        concat!("Set", "tings"),
        concat!("Pocket", "Forge"),
    ];
    for file in files {
        let text = fs::read_to_string(&file).unwrap();
        for term in prohibited {
            assert!(
                !text.contains(term),
                "{} contains prohibited identifier {term}",
                file.display()
            );
        }
    }
}
