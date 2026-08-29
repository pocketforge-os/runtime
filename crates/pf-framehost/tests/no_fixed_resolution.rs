#[test]
fn production_sources_do_not_encode_a_fixed_surface() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    for name in ["pf-render", "pf-framehost"] {
        let src = std::fs::read_dir(root.join(name).join("src")).unwrap();
        for entry in src {
            let path = entry.unwrap().path();
            if path.extension().and_then(|v| v.to_str()) == Some("rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                let forbidden = ["1280", "720"].concat();
                assert!(
                    !text.contains(&forbidden),
                    "{} contains fixed resolution",
                    path.display()
                );
            }
        }
    }
}
