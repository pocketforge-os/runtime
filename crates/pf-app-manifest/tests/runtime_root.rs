use pf_app_manifest::parse_manifest;

const SHIPPED: &str = include_str!("fixtures/steamlink-build58.toml");

#[test]
fn complete_shipped_steamlink_descriptor_is_accepted() {
    parse_manifest(SHIPPED).expect("the production parser must accept the final image descriptor");
}

#[test]
fn runtime_root_metadata_is_strict() {
    for (from, to) in [
        ("[runtime.root]", "[runtime.root]\nunknown = true"),
        ("schema = 1", "schema = 2"),
        ("format = \"squashfs\"", "format = \"ext4\""),
        ("distribution = \"debian-13\"", "distribution = \"unknown\""),
        ("digest = \"sha256:", "digest = \"SHA256:"),
        (
            "platform-runtime = \"pocketforge/bookworm-aarch64\"",
            "platform-runtime = \"../escape\"",
        ),
        (
            "platform-runtime-abi = \"1\"",
            "platform-runtime-abi = \"\"",
        ),
        (
            "platform-runtime-version = \"sha256:",
            "platform-runtime-version = \"SHA256:",
        ),
        ("library-paths = []", "library-paths = [\"../escape\"]"),
        ("library-paths = []", "library-paths = [\"lib\", \"lib\"]"),
        ("[runtime]", "[runtime]\nunknown = true"),
    ] {
        assert!(SHIPPED.contains(from), "fixture lacks {from}");
        assert!(
            parse_manifest(&SHIPPED.replace(from, to)).is_err(),
            "accepted {to}"
        );
    }
}
