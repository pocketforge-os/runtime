use pf_app_launch::{launch_with, required_xdg_paths, ProcessExec};
use pf_app_manifest::{validate_app_id, ReasonCode, Resolver};

fn main() {
    let mut args = std::env::args_os();
    let _program = args.next();
    let Some(app_id) = args.next() else {
        refuse(ReasonCode::InvalidId, "", 64);
    };
    if args.next().is_some() {
        refuse(ReasonCode::InvalidId, &app_id.to_string_lossy(), 64);
    }
    let Some(app_id) = app_id.to_str() else {
        refuse(ReasonCode::InvalidId, &app_id.to_string_lossy(), 65);
    };
    if validate_app_id(app_id).is_err() {
        refuse(ReasonCode::InvalidId, app_id, 65);
    }
    let (config, state) = match required_xdg_paths() {
        Ok(paths) => paths,
        Err(error) => refuse(error.reason(), app_id, error.exit_code()),
    };
    if let Err(error) = launch_with(
        &Resolver::fixed(),
        app_id,
        &config,
        &state,
        &mut ProcessExec,
    ) {
        refuse(error.reason(), app_id, error.exit_code());
    }
}

fn refuse(reason: ReasonCode, item_id: &str, exit_code: u8) -> ! {
    let item_id = serde_json::to_string(item_id).expect("string serialization cannot fail");
    eprintln!(
        "pf-app-launch: launch_refused reason={} item_id={item_id}",
        reason.as_str()
    );
    std::process::exit(i32::from(exit_code));
}
