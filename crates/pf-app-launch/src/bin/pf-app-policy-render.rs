use pf_app_launch::policy::{
    derive_policy, render_systemd_dropin, PolicyInputs, PolicyResourceInventory,
};
use pf_app_manifest::{check_compatibility, parse_manifest, parse_platform_contract, ResolvedApp};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

fn main() {
    if let Err(error) = run() {
        eprintln!("pf-app-policy-render: {error}");
        std::process::exit(65);
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let manifest_path = required(&args, "--manifest")?;
    let platform_contract = required(&args, "--platform-contract")?;
    let resource_inventory = required(&args, "--resource-inventory")?;
    let app_dir = PathBuf::from(required(&args, "--app-dir")?);
    let requested_platform_runtime = PathBuf::from(required(&args, "--platform-runtime")?);
    let xdg_config = PathBuf::from(required(&args, "--xdg-config")?);
    let xdg_state = PathBuf::from(required(&args, "--xdg-state")?);
    let xdg_cache = PathBuf::from(required(&args, "--xdg-cache")?);
    let format = required(&args, "--format")?;

    let manifest_source =
        fs::read_to_string(manifest_path).map_err(|error| format!("read manifest: {error}"))?;
    let manifest = parse_manifest(&manifest_source).map_err(|error| error.to_string())?;
    if app_dir.file_name().and_then(|name| name.to_str()) != Some(&manifest.app.id) {
        return Err("--app-dir basename must equal app.id".to_owned());
    }
    let platform_source = fs::read_to_string(platform_contract)
        .map_err(|error| format!("read platform contract: {error}"))?;
    let platform = parse_platform_contract(&platform_source).map_err(|error| error.to_string())?;
    check_compatibility(&manifest, &platform).map_err(|error| error.to_string())?;
    let executable = app_dir.join(
        &manifest
            .launch
            .as_ref()
            .ok_or_else(|| "launch table is missing".to_owned())?
            .exec,
    );
    let resolved = ResolvedApp {
        id: manifest.app.id.clone(),
        app_dir,
        executable,
        manifest,
        manifest_sha256: format!("{:x}", Sha256::digest(manifest_source.as_bytes())),
        platform,
    };
    let inventory: PolicyResourceInventory = serde_json::from_str(
        &fs::read_to_string(resource_inventory)
            .map_err(|error| format!("read resource inventory: {error}"))?,
    )
    .map_err(|error| format!("parse resource inventory: {error}"))?;
    if inventory.platform_runtime != requested_platform_runtime {
        return Err("--platform-runtime differs from signed resource inventory".to_owned());
    }
    if inventory
        .resources
        .values()
        .flatten()
        .any(|path| !path.is_absolute())
    {
        return Err("resource inventory paths must be absolute".to_owned());
    }
    let policy = derive_policy(
        &resolved,
        &PolicyInputs {
            platform_runtime: inventory.platform_runtime,
            xdg_config,
            xdg_state,
            xdg_cache,
            resource_paths: Vec::new(),
            resources: inventory.resources,
        },
    )
    .map_err(|error| error.to_string())?;

    match format {
        "json" => println!(
            "{}",
            serde_json::to_string_pretty(&policy)
                .map_err(|error| format!("serialize policy: {error}"))?
        ),
        "systemd-dropin" => print!("{}", render_systemd_dropin(&policy)),
        _ => return Err("--format must be json or systemd-dropin".to_owned()),
    }
    Ok(())
}

fn parse_args() -> Result<BTreeMap<String, String>, String> {
    let mut parsed = BTreeMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if !matches!(
            flag.as_str(),
            "--manifest"
                | "--platform-contract"
                | "--resource-inventory"
                | "--app-dir"
                | "--platform-runtime"
                | "--xdg-config"
                | "--xdg-state"
                | "--xdg-cache"
                | "--format"
        ) {
            return Err(format!("unknown argument {flag}"));
        }
        let value = args
            .next()
            .ok_or_else(|| format!("{flag} requires one value"))?;
        if parsed.insert(flag.clone(), value).is_some() {
            return Err(format!("duplicate argument {flag}"));
        }
    }
    Ok(parsed)
}

fn required<'a>(args: &'a BTreeMap<String, String>, flag: &str) -> Result<&'a str, String> {
    args.get(flag)
        .map(String::as_str)
        .ok_or_else(|| format!("missing {flag}"))
}
