use super::*;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "pf-app-manifest-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ))
}

fn platform(path: &Path) -> PathBuf {
    platform_with_capabilities(
        path,
        "platform.toml",
        &["audio", "entropy", "input", "settings"],
    )
}

fn platform_with_capabilities(path: &Path, filename: &str, capabilities: &[&str]) -> PathBuf {
    let contract = path.join(filename);
    let capabilities = capabilities
        .iter()
        .map(|capability| format!("{capability:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(
        &contract,
        format!(
            "schema_version = 1\n\
             runtime_family = \"pocketforge/a133-powervr\"\n\
             runtime_abi = \"1\"\n\
             platform_version = \"20\"\n\
             supported_capabilities = [{capabilities}]\n"
        ),
    )
    .unwrap();
    contract
}

fn install_app(root: &Path, id: &str, manifest_id: &str, exec: &str) -> PathBuf {
    install_app_with_capabilities(root, id, manifest_id, exec, &["audio", "input"])
}

fn install_app_with_capabilities(
    root: &Path,
    id: &str,
    manifest_id: &str,
    exec: &str,
    capabilities: &[&str],
) -> PathBuf {
    let app = root.join(id);
    fs::create_dir_all(app.join("bin")).unwrap();
    let capabilities = capabilities
        .iter()
        .map(|capability| format!("{capability:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(
        app.join("app.toml"),
        format!(
            "[app]\nid = \"{manifest_id}\"\nname = \"Test\"\nuse = [{capabilities}]\n\
             [runtime]\nfamily = \"pocketforge/a133-powervr\"\nabi = \"1\"\nplatform-version = \"20\"\n\
             [launch]\nexec = \"{exec}\"\n"
        ),
    )
    .unwrap();
    let executable = app.join("bin/app");
    fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&executable, permissions).unwrap();
    app
}

#[test]
fn id_grammar_is_exact() {
    for valid in [
        "a.b",
        "org.example.app",
        "org.pocket_forge.app-2",
        &format!("a.{}", "b".repeat(198)),
    ] {
        assert_eq!(validate_app_id(valid), Ok(()), "{valid}");
    }
    for invalid in [
        "../outside",
        "org..example",
        "/absolute",
        "Org.example",
        "-org.example",
        "org-.example",
        "org.example_",
        "single",
        "a.b/other",
        &format!("a.{}", "b".repeat(199)),
    ] {
        assert_eq!(validate_app_id(invalid), Err(InvalidAppId), "{invalid}");
    }
}

#[test]
fn stable_reason_code_spellings_are_frozen() {
    assert_eq!(
        [
            ReasonCode::InvalidId,
            ReasonCode::AppRootMissing,
            ReasonCode::AppRootSymlink,
            ReasonCode::AppNotFound,
            ReasonCode::AppDirNotDirectory,
            ReasonCode::AppDirSymlink,
            ReasonCode::AppDirEscape,
            ReasonCode::DescriptorMissing,
            ReasonCode::DescriptorNotRegular,
            ReasonCode::DescriptorSymlink,
            ReasonCode::DescriptorParse,
            ReasonCode::DescriptorInvalid,
            ReasonCode::DescriptorIdMismatch,
            ReasonCode::LaunchMissing,
            ReasonCode::LaunchExecInvalid,
            ReasonCode::ExecMissing,
            ReasonCode::ExecNotRegular,
            ReasonCode::ExecSymlink,
            ReasonCode::ExecEscape,
            ReasonCode::ExecNotExecutable,
            ReasonCode::ExecIo,
            ReasonCode::RuntimeFamilyMismatch,
            ReasonCode::RuntimeAbiMismatch,
            ReasonCode::PlatformVersionMismatch,
            ReasonCode::UnsupportedCapability,
            ReasonCode::RequirementUnverifiable,
            ReasonCode::InsufficientMemory,
            ReasonCode::UnsupportedGraphics,
            ReasonCode::MissingInput,
            ReasonCode::DisplayTooSmall,
            ReasonCode::PlatformContractMissing,
            ReasonCode::PlatformContractInvalid,
            ReasonCode::SystemdStartFailed,
            ReasonCode::SystemdStateUnknown,
            ReasonCode::AppExitFailed,
            ReasonCode::TargetNotReleased,
            ReasonCode::OwnerNotActive,
            ReasonCode::PresentationNotAcknowledged,
        ]
        .map(ReasonCode::as_str),
        [
            "invalid_id",
            "app_root_missing",
            "app_root_symlink",
            "app_not_found",
            "app_dir_not_directory",
            "app_dir_symlink",
            "app_dir_escape",
            "descriptor_missing",
            "descriptor_not_regular",
            "descriptor_symlink",
            "descriptor_parse",
            "descriptor_invalid",
            "descriptor_id_mismatch",
            "launch_missing",
            "launch_exec_invalid",
            "exec_missing",
            "exec_not_regular",
            "exec_symlink",
            "exec_escape",
            "exec_not_executable",
            "exec_io",
            "runtime_family_mismatch",
            "runtime_abi_mismatch",
            "platform_version_mismatch",
            "unsupported_capability",
            "requirement_unverifiable",
            "insufficient_memory",
            "unsupported_graphics",
            "missing_input",
            "display_too_small",
            "platform_contract_missing",
            "platform_contract_invalid",
            "systemd_start_failed",
            "systemd_state_unknown",
            "app_exit_failed",
            "target_not_released",
            "owner_not_active",
            "presentation_not_acknowledged",
        ]
    );
    assert_eq!(
        [
            PlatformContractErrorReason::Missing,
            PlatformContractErrorReason::Read,
            PlatformContractErrorReason::Parse,
            PlatformContractErrorReason::Schema,
            PlatformContractErrorReason::InvalidFamily,
            PlatformContractErrorReason::InvalidAbi,
            PlatformContractErrorReason::InvalidPlatformVersion,
            PlatformContractErrorReason::InvalidCapability,
            PlatformContractErrorReason::DuplicateCapability,
            PlatformContractErrorReason::UnsortedCapabilities,
        ]
        .map(PlatformContractErrorReason::as_str),
        [
            "missing",
            "read",
            "parse",
            "schema",
            "invalid_family",
            "invalid_abi",
            "invalid_platform_version",
            "invalid_capability",
            "duplicate_capability",
            "unsorted_capabilities",
        ]
    );
}

#[test]
fn semantic_capability_duplicates_and_positive_share_one_invocation() {
    let dir = scratch("capability-duplicates");
    let root = dir.join("apps");
    fs::create_dir_all(&root).unwrap();
    let contract = platform(&dir);
    install_app_with_capabilities(
        &root,
        "org.example.good",
        "org.example.good",
        "bin/app",
        &["audio"],
    );
    install_app_with_capabilities(
        &root,
        "org.example.optional",
        "org.example.optional",
        "bin/app",
        &["audio", "audio?"],
    );
    install_app_with_capabilities(
        &root,
        "org.example.modifiers",
        "org.example.modifiers",
        "bin/app",
        &["location:approximate", "location:precise"],
    );

    let resolver = Resolver::new(&root, contract);
    assert!(resolver.resolve("org.example.good").is_ok());
    for id in ["org.example.optional", "org.example.modifiers"] {
        let error = resolver.resolve(id).unwrap_err();
        assert_eq!(error.reason, ReasonCode::DescriptorInvalid, "{id}");
        assert_eq!(error.detail, "invalid or duplicate capability", "{id}");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn normalized_capability_compatibility_and_controls_share_one_invocation() {
    let dir = scratch("normalized-capability-compatibility");
    let root = dir.join("apps");
    fs::create_dir_all(&root).unwrap();
    let supported_contract =
        platform_with_capabilities(&dir, "platform-supported.toml", &["audio"]);
    let unsupported_contract = platform_with_capabilities(&dir, "platform-unsupported.toml", &[]);

    for (id, capabilities) in [
        ("org.example.control", &["audio"][..]),
        ("org.example.uppercase", &["Audio"][..]),
        ("org.example.spaced", &[" audio "][..]),
        ("org.example.optional", &["audio?"][..]),
    ] {
        install_app_with_capabilities(&root, id, id, "bin/app", capabilities);
    }

    let supported = Resolver::new(&root, supported_contract);
    for id in [
        "org.example.control",
        "org.example.uppercase",
        "org.example.spaced",
    ] {
        assert!(supported.resolve(id).is_ok(), "{id}");
    }

    let unsupported = Resolver::new(&root, unsupported_contract);
    assert!(unsupported.resolve("org.example.optional").is_ok());
    let error = unsupported.resolve("org.example.control").unwrap_err();
    assert_eq!(error.reason, ReasonCode::UnsupportedCapability);
    assert_eq!(error.detail, "unsupported required capability audio");

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn path_confinement_negatives_and_positive_share_one_invocation() {
    let dir = scratch("coordinator-controls");
    let root = dir.join("apps");
    fs::create_dir_all(&root).unwrap();
    let contract = platform(&dir);
    install_app(&root, "org.example.good", "org.example.good", "bin/./app");

    let outside = dir.join("outside");
    fs::create_dir_all(&outside).unwrap();
    install_app(&dir, "outside", "org.example.link", "bin/app");
    symlink(&outside, root.join("org.example.link")).unwrap();
    install_app(
        &root,
        "org.example.mismatch",
        "org.example.other",
        "bin/app",
    );

    let resolver = Resolver::new(&root, contract);
    assert_eq!(
        resolver.resolve("org.example.good").unwrap().executable,
        fs::canonicalize(root.join("org.example.good/bin/app")).unwrap()
    );
    assert_eq!(
        resolver.resolve("../outside").unwrap_err().reason,
        ReasonCode::InvalidId
    );
    assert_eq!(
        resolver.resolve("org.example.link").unwrap_err().reason,
        ReasonCode::AppDirSymlink
    );
    assert_eq!(
        resolver.resolve("org.example.mismatch").unwrap_err().reason,
        ReasonCode::DescriptorIdMismatch
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn descriptor_and_executable_confinement_errors_are_stable() {
    let dir = scratch("files");
    let root = dir.join("apps");
    fs::create_dir_all(&root).unwrap();
    let contract = platform(&dir);

    let descriptor_link = root.join("org.example.descriptor");
    fs::create_dir_all(&descriptor_link).unwrap();
    fs::write(dir.join("outside.toml"), b"").unwrap();
    symlink(dir.join("outside.toml"), descriptor_link.join("app.toml")).unwrap();

    install_app(
        &root,
        "org.example.absolute",
        "org.example.absolute",
        "/bin/sh",
    );
    install_app(
        &root,
        "org.example.parent",
        "org.example.parent",
        "../outside",
    );
    install_app(
        &root,
        "org.example.arguments",
        "org.example.arguments",
        "bin/app --flag",
    );
    install_app(
        &root,
        "org.example.shell",
        "org.example.shell",
        "bin/app;true",
    );
    let symlink_app = install_app(
        &root,
        "org.example.exec-link",
        "org.example.exec-link",
        "bin/app",
    );
    fs::remove_file(symlink_app.join("bin/app")).unwrap();
    fs::write(dir.join("outside-exec"), b"x").unwrap();
    symlink(dir.join("outside-exec"), symlink_app.join("bin/app")).unwrap();
    let non_regular = install_app(
        &root,
        "org.example.directory",
        "org.example.directory",
        "bin/app",
    );
    fs::remove_file(non_regular.join("bin/app")).unwrap();
    fs::create_dir(non_regular.join("bin/app")).unwrap();
    let escaped = install_app(&root, "org.example.escape", "org.example.escape", "bin/app");
    fs::remove_dir_all(escaped.join("bin")).unwrap();
    let outside_bin = dir.join("outside-bin");
    fs::create_dir_all(&outside_bin).unwrap();
    let outside_exec = outside_bin.join("app");
    fs::write(&outside_exec, b"x").unwrap();
    let mut outside_permissions = fs::metadata(&outside_exec).unwrap().permissions();
    outside_permissions.set_mode(0o755);
    fs::set_permissions(&outside_exec, outside_permissions).unwrap();
    symlink(&outside_bin, escaped.join("bin")).unwrap();
    let non_exec = install_app(&root, "org.example.noexec", "org.example.noexec", "bin/app");
    let mut permissions = fs::metadata(non_exec.join("bin/app"))
        .unwrap()
        .permissions();
    permissions.set_mode(0o644);
    fs::set_permissions(non_exec.join("bin/app"), permissions).unwrap();

    let resolver = Resolver::new(&root, contract);
    for (id, reason, exit) in [
        ("org.example.descriptor", ReasonCode::DescriptorSymlink, 66),
        ("org.example.absolute", ReasonCode::LaunchExecInvalid, 65),
        ("org.example.parent", ReasonCode::LaunchExecInvalid, 65),
        ("org.example.arguments", ReasonCode::LaunchExecInvalid, 65),
        ("org.example.shell", ReasonCode::LaunchExecInvalid, 65),
        ("org.example.exec-link", ReasonCode::ExecSymlink, 66),
        ("org.example.directory", ReasonCode::ExecNotRegular, 66),
        ("org.example.escape", ReasonCode::ExecEscape, 65),
        ("org.example.noexec", ReasonCode::ExecNotExecutable, 126),
    ] {
        let error = resolver.resolve(id).unwrap_err();
        assert_eq!(error.reason, reason, "{id}");
        assert_eq!(error.helper_exit_code(), exit, "{id}");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn platform_contract_is_exact_and_compatibility_is_enforced() {
    let source = "schema_version = 1\n\
                  runtime_family = \"pocketforge/a133-powervr\"\n\
                  runtime_abi = \"1\"\n\
                  platform_version = \"20\"\n\
                  supported_capabilities = [\"audio\", \"input\"]\n";
    assert_eq!(parse_platform_contract(source).unwrap().schema_version, 1);
    for (replacement, reason) in [
        (
            "supported_capabilities = [\"input\", \"audio\"]",
            PlatformContractErrorReason::UnsortedCapabilities,
        ),
        (
            "supported_capabilities = [\"audio\", \"audio\"]",
            PlatformContractErrorReason::DuplicateCapability,
        ),
        (
            "supported_capabilities = [\"audio\", \"telepathy\"]",
            PlatformContractErrorReason::InvalidCapability,
        ),
    ] {
        let candidate = source.replace(
            "supported_capabilities = [\"audio\", \"input\"]",
            replacement,
        );
        assert_eq!(
            parse_platform_contract(&candidate).unwrap_err().reason,
            reason
        );
    }
    assert_eq!(
        parse_platform_contract(&format!("{source}unknown = true\n"))
            .unwrap_err()
            .reason,
        PlatformContractErrorReason::Schema
    );

    let base_manifest = "[app]\nid = \"org.example.app\"\nuse = [\"audio\"]\n\
                         [runtime]\nfamily = \"pocketforge/a133-powervr\"\nabi = \"1\"\nplatform-version = \"20\"\n\
                         [launch]\nexec = \"bin/app\"\n";
    let contract = parse_platform_contract(source).unwrap();
    for (manifest_source, reason) in [
        (
            base_manifest.replace("a133-powervr", "a523-mali"),
            ReasonCode::RuntimeFamilyMismatch,
        ),
        (
            base_manifest.replace("abi = \"1\"", "abi = \"2\""),
            ReasonCode::RuntimeAbiMismatch,
        ),
        (
            base_manifest.replace("platform-version = \"20\"", "platform-version = \"21\""),
            ReasonCode::PlatformVersionMismatch,
        ),
        (
            base_manifest.replace("use = [\"audio\"]", "use = [\"settings\"]"),
            ReasonCode::UnsupportedCapability,
        ),
    ] {
        let manifest = parse_manifest(&manifest_source).unwrap();
        assert_eq!(
            check_compatibility(&manifest, &contract)
                .unwrap_err()
                .reason,
            reason
        );
    }
}

/// A schema-2 fact block; `extra` inserts additional scalar facts before the
/// trailing `[display]` table (TOML tables must come after every scalar).
fn v2_facts(extra: &str) -> String {
    format!(
        "physical_memory_mib = 970\n\
         app_memory_budget_mib = 384\n\
         gles_max = \"3.0\"\n\
         {extra}\
         inputs = [\"buttons\", \"dpad\", \"two_sticks\"]\n\
         \n\
         [display]\n\
         width = 1280\n\
         height = 720\n"
    )
}

fn contract_v2(facts: &str) -> String {
    format!(
        "schema_version = 2\n\
         runtime_family = \"pocketforge/a133-powervr\"\n\
         runtime_abi = \"1\"\n\
         platform_version = \"20\"\n\
         supported_capabilities = [\"audio\", \"input\"]\n\
         {facts}"
    )
}

fn manifest_with(table: &str) -> String {
    format!(
        "[app]\nid = \"org.example.app\"\nuse = [\"audio\"]\n\
         [runtime]\nfamily = \"pocketforge/a133-powervr\"\nabi = \"1\"\nplatform-version = \"20\"\n\
         [launch]\nexec = \"bin/app\"\n{table}"
    )
}

fn check(table: &str, contract: &PlatformContract) -> Result<(), ResolveError> {
    let manifest = parse_manifest(&manifest_with(table)).unwrap();
    check_compatibility(&manifest, contract)
}

const SCHEMA1_SOURCE: &str = "schema_version = 1\n\
                              runtime_family = \"pocketforge/a133-powervr\"\n\
                              runtime_abi = \"1\"\n\
                              platform_version = \"20\"\n\
                              supported_capabilities = [\"audio\", \"input\"]\n";

#[test]
fn insufficient_memory_refuses_with_positive_control_in_one_invocation() {
    let contract = parse_platform_contract(&contract_v2(&v2_facts(""))).unwrap();
    // Positive control: exactly at the budget passes.
    assert!(check("[requirements]\nmemory_mib = 384\n", &contract).is_ok());
    // One MiB over refuses.
    let error = check("[requirements]\nmemory_mib = 385\n", &contract).unwrap_err();
    assert_eq!(error.reason, ReasonCode::InsufficientMemory);
    assert_eq!(error.helper_exit_code(), 65);
    assert_eq!(
        error.detail,
        "memory_mib 385 exceeds app_memory_budget_mib 384"
    );
}

#[test]
fn unsupported_graphics_refuses_with_positive_controls_in_one_invocation() {
    let contract =
        parse_platform_contract(&contract_v2(&v2_facts("vulkan_max = \"1.1\"\n"))).unwrap();
    // Positive controls: exactly at the platform maximum passes.
    assert!(check("[requirements]\ngles = \"3.0\"\n", &contract).is_ok());
    assert!(check("[requirements]\nvulkan = \"1.1\"\n", &contract).is_ok());
    assert!(check("[requirements]\ngles = \"2.0\"\n", &contract).is_ok());
    // Above the maximum refuses, naming both versions.
    let error = check("[requirements]\ngles = \"3.1\"\n", &contract).unwrap_err();
    assert_eq!(error.reason, ReasonCode::UnsupportedGraphics);
    assert_eq!(error.helper_exit_code(), 65);
    assert_eq!(error.detail, "gles 3.1 exceeds gles_max 3.0");
    let error = check("[requirements]\nvulkan = \"1.2\"\n", &contract).unwrap_err();
    assert_eq!(error.reason, ReasonCode::UnsupportedGraphics);
    assert_eq!(error.detail, "vulkan 1.2 exceeds vulkan_max 1.1");
}

#[test]
fn missing_input_refuses_with_positive_controls_in_one_invocation() {
    let contract = parse_platform_contract(&contract_v2(&v2_facts(""))).unwrap();
    // Positive controls: declared inputs pass, two_sticks implies stick, and
    // keyboard/pointer are satisfied by the system input layer on every device.
    for table in [
        "[requirements]\ninputs = [\"buttons\"]\n",
        "[requirements]\ninputs = [\"stick\"]\n",
        "[requirements]\ninputs = [\"two_sticks\"]\n",
        "[requirements]\ninputs = [\"keyboard\", \"pointer\"]\n",
    ] {
        assert!(check(table, &contract).is_ok(), "{table}");
    }
    // An input the device does not have refuses.
    let error = check("[requirements]\ninputs = [\"touch\"]\n", &contract).unwrap_err();
    assert_eq!(error.reason, ReasonCode::MissingInput);
    assert_eq!(error.helper_exit_code(), 65);
    assert_eq!(
        error.detail,
        "required input touch is not among platform inputs buttons, dpad, two_sticks"
    );
}

#[test]
fn display_too_small_refuses_with_positive_controls_in_one_invocation() {
    let contract = parse_platform_contract(&contract_v2(&v2_facts(""))).unwrap();
    // Positive controls: exact fit passes, and either orientation passes
    // because the comparison is orientation-independent (sorted sides).
    for table in [
        "[requirements.display_min]\nwidth = 1280\nheight = 720\n",
        "[requirements.display_min]\nwidth = 720\nheight = 1280\n",
        "[requirements.display_min]\nwidth = 480\nheight = 640\n",
    ] {
        assert!(check(table, &contract).is_ok(), "{table}");
    }
    // One physical pixel over on either sorted side refuses.
    let error = check(
        "[requirements.display_min]\nwidth = 1281\nheight = 720\n",
        &contract,
    )
    .unwrap_err();
    assert_eq!(error.reason, ReasonCode::DisplayTooSmall);
    assert_eq!(error.helper_exit_code(), 65);
    assert_eq!(
        error.detail,
        "display_min 1281x720 exceeds display 1280x720"
    );
    let error = check(
        "[requirements.display_min]\nwidth = 720\nheight = 1281\n",
        &contract,
    )
    .unwrap_err();
    assert_eq!(error.reason, ReasonCode::DisplayTooSmall);
}

#[test]
fn requirements_on_schema_1_contracts_fail_closed_and_absent_requirements_are_unchanged() {
    let contract = parse_platform_contract(SCHEMA1_SOURCE).unwrap();
    // Positive controls: a schema-1 contract with no requirements (or only the
    // informational recommended table) keeps the historical behaviour.
    assert!(check("", &contract).is_ok());
    assert!(check("[recommended]\nmemory_mib = 512\n", &contract).is_ok());
    // Every declared requirement is unverifiable against schema 1: fail closed.
    for (table, detail) in [
        (
            "[requirements]\nmemory_mib = 512\n",
            "memory_mib 512 is unverifiable: platform contract declares no app_memory_budget_mib",
        ),
        (
            "[requirements]\ngles = \"2.0\"\n",
            "gles 2.0 is unverifiable: platform contract declares no gles_max",
        ),
        (
            "[requirements]\nvulkan = \"1.0\"\n",
            "vulkan 1.0 is unverifiable: platform contract declares no vulkan_max",
        ),
        (
            "[requirements]\ninputs = [\"buttons\"]\n",
            "input buttons is unverifiable: platform contract declares no inputs",
        ),
        (
            "[requirements.display_min]\nwidth = 640\nheight = 480\n",
            "display_min 640x480 is unverifiable: platform contract declares no display",
        ),
    ] {
        let error = check(table, &contract).unwrap_err();
        assert_eq!(error.reason, ReasonCode::RequirementUnverifiable, "{table}");
        assert_eq!(error.helper_exit_code(), 65, "{table}");
        assert_eq!(error.detail, detail, "{table}");
    }
    // A schema-2 contract without the optional graphics maxima fails closed too.
    let no_graphics = parse_platform_contract(&contract_v2(&v2_facts(""))).unwrap();
    let error = check("[requirements]\nvulkan = \"1.0\"\n", &no_graphics).unwrap_err();
    assert_eq!(error.reason, ReasonCode::RequirementUnverifiable);
    assert_eq!(
        error.detail,
        "vulkan 1.0 is unverifiable: platform contract declares no vulkan_max"
    );
}

#[test]
fn recommended_requirements_never_refuse() {
    let contract =
        parse_platform_contract(&contract_v2(&v2_facts("vulkan_max = \"1.1\"\n"))).unwrap();
    let table = "[recommended]\n\
                 memory_mib = 65535\n\
                 gles = \"3.2\"\n\
                 vulkan = \"1.4\"\n\
                 inputs = [\"touch\"]\n\
                 storage_mib = 4294967295\n\
                 \n\
                 [recommended.display_min]\nwidth = 7680\nheight = 4320\n";
    assert!(check(table, &contract).is_ok());
    // The recommended table is still validated strictly.
    let error = parse_manifest(&manifest_with("[recommended]\ngles = \"2.5\"\n")).unwrap_err();
    assert_eq!(error.kind, ManifestErrorKind::Invalid);
    assert_eq!(error.message, "invalid recommended.gles");
}

#[test]
fn requirements_tables_parse_strictly() {
    // Versions come from the fixed vocabulary, exactly "major.minor".
    for (table, message) in [
        (
            "[requirements]\ngles = \"2.5\"\n",
            "invalid requirements.gles",
        ),
        (
            "[requirements]\ngles = \"3\"\n",
            "invalid requirements.gles",
        ),
        (
            "[requirements]\ngles = \"3.0.1\"\n",
            "invalid requirements.gles",
        ),
        ("[requirements]\ngles = \"\"\n", "invalid requirements.gles"),
        (
            "[requirements]\nvulkan = \"1.5\"\n",
            "invalid requirements.vulkan",
        ),
        (
            "[requirements]\nvulkan = \"1.10\"\n",
            "invalid requirements.vulkan",
        ),
        (
            "[requirements]\ninputs = [\"telepathy\"]\n",
            "invalid requirements input",
        ),
        (
            "[requirements]\ninputs = [\"buttons\", \"buttons\"]\n",
            "duplicate requirements input",
        ),
    ] {
        let error = parse_manifest(&manifest_with(table)).unwrap_err();
        assert_eq!(error.kind, ManifestErrorKind::Invalid, "{table}");
        assert_eq!(error.message, message, "{table}");
    }
    // Unknown fields are rejected by deny_unknown_fields.
    let error = parse_manifest(&manifest_with("[requirements]\ncpu_mhz = 1000\n")).unwrap_err();
    assert_eq!(error.kind, ManifestErrorKind::Invalid);
    // The full vocabulary parses, and every field is optional.
    let manifest = parse_manifest(&manifest_with(
        "[requirements]\n\
         memory_mib = 256\n\
         gles = \"2.0\"\n\
         vulkan = \"1.0\"\n\
         inputs = [\"buttons\", \"dpad\", \"stick\", \"two_sticks\", \"touch\"]\n\
         storage_mib = 64\n\
         \n\
         [requirements.display_min]\nwidth = 640\nheight = 480\n",
    ))
    .unwrap();
    let requirements = manifest.requirements.unwrap();
    assert_eq!(requirements.memory_mib, Some(256));
    assert_eq!(requirements.storage_mib, Some(64));
    assert_eq!(
        requirements.display_min,
        Some(DisplaySize {
            width: 640,
            height: 480
        })
    );
    assert!(manifest.recommended.is_none());
    // storage_mib is carried but never checked at launch.
    let contract = parse_platform_contract(&contract_v2(&v2_facts(""))).unwrap();
    assert!(check("[requirements]\nstorage_mib = 4294967295\n", &contract).is_ok());
    // keyboard/pointer warn (not error): the system input layer provides them.
    let manifest = parse_manifest(&manifest_with(
        "[requirements]\ninputs = [\"keyboard\", \"pointer\"]\n\n[recommended]\ninputs = [\"keyboard\"]\n",
    ))
    .unwrap();
    assert_eq!(
        manifest_warnings(&manifest),
        [
            "requirements input keyboard is provided by the system input layer on every device",
            "requirements input pointer is provided by the system input layer on every device",
            "recommended input keyboard is provided by the system input layer on every device",
        ]
    );
    // Without system inputs there are no warnings.
    assert!(manifest_warnings(&parse_manifest(&manifest_with("")).unwrap()).is_empty());
}

#[test]
fn platform_contract_v2_round_trips_and_validates_strictly() {
    let source = contract_v2(&v2_facts("vulkan_max = \"1.1\"\n"));
    let contract = parse_platform_contract(&source).unwrap();
    assert_eq!(contract.schema_version, 2);
    assert_eq!(contract.physical_memory_mib, Some(970));
    assert_eq!(contract.app_memory_budget_mib, Some(384));
    assert_eq!(contract.gles_max.as_deref(), Some("3.0"));
    assert_eq!(contract.vulkan_max.as_deref(), Some("1.1"));
    assert_eq!(
        contract.inputs,
        Some(vec![
            "buttons".to_owned(),
            "dpad".to_owned(),
            "two_sticks".to_owned()
        ])
    );
    assert_eq!(
        contract.display,
        Some(DisplaySize {
            width: 1280,
            height: 720
        })
    );
    // Round-trip: serialize and re-parse to an identical contract.
    let serialized = toml::to_string(&contract).unwrap();
    assert_eq!(parse_platform_contract(&serialized).unwrap(), contract);

    // Negative controls: structural violations fail closed with Schema.
    for (candidate, message) in [
        (
            contract_v2(&v2_facts("")).replace("schema_version = 2", "schema_version = 3"),
            "schema_version must be 1 or 2",
        ),
        (
            contract_v2(&v2_facts("")).replace("physical_memory_mib = 970\n", ""),
            "schema_version 2 requires physical_memory_mib",
        ),
        (
            contract_v2(&v2_facts("")).replace("app_memory_budget_mib = 384\n", ""),
            "schema_version 2 requires app_memory_budget_mib",
        ),
        (
            contract_v2(&v2_facts(""))
                .replace("inputs = [\"buttons\", \"dpad\", \"two_sticks\"]\n", ""),
            "schema_version 2 requires inputs",
        ),
        (
            contract_v2(&v2_facts("")).replace("[display]\nwidth = 1280\nheight = 720\n", ""),
            "schema_version 2 requires display",
        ),
        (
            format!("{SCHEMA1_SOURCE}physical_memory_mib = 970\n"),
            "schema_version 1 must not declare schema 2 fields",
        ),
        (
            format!("{SCHEMA1_SOURCE}inputs = []\n"),
            "schema_version 1 must not declare schema 2 fields",
        ),
        (
            contract_v2(&v2_facts("")).replace("gles_max = \"3.0\"", "gles_max = \"4.0\""),
            "invalid gles_max",
        ),
        (
            contract_v2(&v2_facts("vulkan_max = \"1.1\"\n"))
                .replace("vulkan_max = \"1.1\"", "vulkan_max = \"2.0\""),
            "invalid vulkan_max",
        ),
        (
            contract_v2(&v2_facts("")).replace(
                "inputs = [\"buttons\", \"dpad\", \"two_sticks\"]",
                "inputs = [\"telepathy\"]",
            ),
            "unsupported input name",
        ),
    ] {
        let error = parse_platform_contract(&candidate).unwrap_err();
        assert_eq!(
            error.reason,
            PlatformContractErrorReason::Schema,
            "{message}"
        );
        assert_eq!(error.message, message, "{candidate}");
    }
    // Positive control: schema 1 without the new facts still parses.
    assert_eq!(
        parse_platform_contract(SCHEMA1_SOURCE)
            .unwrap()
            .schema_version,
        1
    );
}

#[test]
fn resolver_surfaces_new_reason_codes_end_to_end() {
    let dir = scratch("minimum-spec");
    let root = dir.join("apps");
    fs::create_dir_all(&root).unwrap();
    let contract = dir.join("platform.toml");
    fs::write(&contract, contract_v2(&v2_facts(""))).unwrap();

    for (id, requirements) in [
        ("org.example.fits", "[requirements]\nmemory_mib = 384\n"),
        ("org.example.over", "[requirements]\nmemory_mib = 385\n"),
    ] {
        let app = root.join(id);
        fs::create_dir_all(app.join("bin")).unwrap();
        fs::write(
            app.join("app.toml"),
            format!(
                "[app]\nid = \"{id}\"\nuse = [\"audio\"]\n\
                 [runtime]\nfamily = \"pocketforge/a133-powervr\"\nabi = \"1\"\nplatform-version = \"20\"\n\
                 [launch]\nexec = \"bin/app\"\n{requirements}"
            ),
        )
        .unwrap();
        let executable = app.join("bin/app");
        fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
    }

    let resolver = Resolver::new(&root, &contract);
    assert!(resolver.resolve("org.example.fits").is_ok());
    let error = resolver.resolve("org.example.over").unwrap_err();
    assert_eq!(error.reason, ReasonCode::InsufficientMemory);
    assert_eq!(error.helper_exit_code(), 65);
    // The refusal surface prints reason.as_str(), so the frozen spelling is
    // exactly what the launcher and session authority log.
    assert_eq!(error.reason.as_str(), "insufficient_memory");
    fs::remove_dir_all(dir).unwrap();
}
