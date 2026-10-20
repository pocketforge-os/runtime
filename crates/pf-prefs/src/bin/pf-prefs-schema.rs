use std::path::Path;

use pf_prefs::schema::{seed_schema_json, PREFS_SCHEMA_PATH};

fn main() {
    let output = std::env::args()
        .nth(1)
        .unwrap_or_else(|| PREFS_SCHEMA_PATH.to_owned());
    let text = seed_schema_json();

    if output == "-" {
        print!("{text}");
        return;
    }

    let path = Path::new(&output);
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|error| panic!("create {}: {error}", parent.display()));
    }
    std::fs::write(path, text).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
}
