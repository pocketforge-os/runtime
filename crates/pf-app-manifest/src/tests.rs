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
    let contract = path.join("platform.toml");
    fs::write(
        &contract,
        "schema_version = 1\n\
         runtime_family = \"pocketforge/a133-powervr\"\n\
         runtime_abi = \"1\"\n\
         platform_version = \"20\"\n\
         supported_capabilities = [\"audio\", \"entropy\", \"input\", \"settings\"]\n",
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
