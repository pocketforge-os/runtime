use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use pf_session_contract::{
    SessionContractError, SessionPublication, SessionPublisher, DEFAULT_SESSION_ROOT,
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("pf-session-contract-{id}-{}", std::process::id()));
        fs::create_dir(&path).expect("unique scratch directory");
        Self { path }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn source_file(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, b"hermetic endpoint").expect("source endpoint");
    path
}

fn publication(dir: &Path) -> SessionPublication {
    SessionPublication::new("wayland-0", source_file(dir, "wayland-0"))
        .with_xwayland(":0", source_file(dir, "xauthority"))
        .with_capability("audio", source_file(dir, "audio.sock"))
        .and_then(|publication| {
            publication.with_capability("settings", source_file(dir, "settings.sock"))
        })
        .expect("allowed capability")
}

#[test]
fn partial_publication_is_not_ready() {
    let scratch = Scratch::new();
    let root = scratch.path("session");
    let generation = scratch.path(".session-generations/1");
    fs::create_dir_all(&generation).unwrap();
    fs::write(generation.join("generation"), b"1\n").unwrap();
    fs::write(
        generation.join("environment"),
        format!(
            "POCKETFORGE_SESSION={}\nPOCKETFORGE_SESSION_GENERATION=1\nWAYLAND_DISPLAY=wayland-0\n",
            root.display()
        ),
    )
    .unwrap();
    symlink(".session-generations/1", &root).unwrap();

    let result = SessionPublisher::new(&root).read();
    assert!(matches!(result, Err(SessionContractError::NotReady { .. })));
}

#[test]
fn publication_exposes_environment_readiness_and_generation_as_one_atomic_snapshot() {
    let scratch = Scratch::new();
    let root = scratch.path("session");
    let publisher = SessionPublisher::new(&root);

    let generation = publisher.publish(&publication(&scratch.path)).unwrap();
    assert_eq!(generation, 1);
    assert!(fs::symlink_metadata(&root)
        .unwrap()
        .file_type()
        .is_symlink());

    let snapshot = publisher.read().unwrap();
    assert_eq!(snapshot.generation, 1);
    assert_eq!(snapshot.wayland_display, "wayland-0");
    assert_eq!(snapshot.display.as_deref(), Some(":0"));
    assert_eq!(
        snapshot.xauthority.as_deref(),
        Some(scratch.path("xauthority").as_path())
    );
    assert_eq!(
        snapshot.capabilities.get("audio"),
        Some(&scratch.path("audio.sock"))
    );
    assert_eq!(
        fs::read_to_string(root.join("readiness")).unwrap(),
        "ready\n"
    );
    assert_eq!(fs::read_to_string(root.join("generation")).unwrap(), "1\n");
    assert_eq!(
        fs::read_to_string(root.join("environment")).unwrap(),
        format!(
            "POCKETFORGE_SESSION={}\nPOCKETFORGE_SESSION_GENERATION=1\nWAYLAND_DISPLAY=wayland-0\nDISPLAY=:0\nXAUTHORITY={}/xauthority\n",
            root.display(),
            root.display()
        )
    );
}

#[test]
fn stale_generation_is_rejected_and_reconnect_reads_the_new_snapshot() {
    let scratch = Scratch::new();
    let root = scratch.path("session");
    let publisher = SessionPublisher::new(&root);

    let first = publisher.publish(&publication(&scratch.path)).unwrap();
    assert_eq!(publisher.reconnect(first).unwrap().generation, first);
    let second = publisher.publish(&publication(&scratch.path)).unwrap();
    assert_eq!(second, first + 1);

    assert!(matches!(
        publisher.reconnect(first),
        Err(SessionContractError::StaleGeneration { expected, actual }) if expected == first && actual == second
    ));
    assert_eq!(publisher.reconnect(second).unwrap().generation, second);
}

#[test]
fn app_root_projection_is_filtered_rooted_and_read_only() {
    let scratch = Scratch::new();
    let root = scratch.path("session");
    let app_root = scratch.path("app-root");
    let publisher = SessionPublisher::new(&root);
    publisher.publish(&publication(&scratch.path)).unwrap();

    publisher.project_app_root(&app_root, &["audio"]).unwrap();
    let projected = app_root.join("run/pocketforge/session");
    assert!(projected.is_dir());
    assert!(projected.join("environment").exists());
    assert!(projected.join("readiness").exists());
    assert!(projected.join("generation").exists());
    assert!(projected.join("wayland-0").exists());
    assert!(projected.join("capabilities/audio").exists());
    assert!(!projected.join("capabilities/missing").exists());
    assert!(!projected.join("capabilities/settings").exists());
    assert_eq!(
        fs::metadata(projected.join("environment"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o444
    );
    assert_eq!(
        fs::metadata(&projected).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(projected.starts_with(&app_root));
}

#[test]
fn app_root_projection_remains_pinned_when_session_is_republished() {
    let scratch = Scratch::new();
    let root = scratch.path("session");
    let app_root = scratch.path("app-root");
    let publisher = SessionPublisher::new(&root);
    let first_wayland = source_file(&scratch.path, "first-wayland.sock");
    let first_generation = publisher
        .publish(
            &SessionPublication::new("wayland-0", &first_wayland)
                .with_xwayland(":0", source_file(&scratch.path, "first-xauthority")),
        )
        .unwrap();

    publisher.project_app_root(&app_root, &[]).unwrap();
    let projected = app_root.join("run/pocketforge/session");
    assert!(fs::symlink_metadata(&projected)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        fs::read_to_string(projected.join("generation")).unwrap(),
        format!("{first_generation}\n")
    );
    assert_eq!(
        fs::canonicalize(projected.join("wayland-0")).unwrap(),
        fs::canonicalize(&first_wayland).unwrap()
    );

    let second_wayland = source_file(&scratch.path, "second-wayland.sock");
    let second_generation = publisher
        .publish(
            &SessionPublication::new("wayland-0", &second_wayland)
                .with_xwayland(":0", source_file(&scratch.path, "second-xauthority")),
        )
        .unwrap();
    assert_eq!(second_generation, first_generation + 1);
    assert_eq!(publisher.read().unwrap().generation, second_generation);

    // The existing app projection is immutable and continues to resolve to the generation it
    // published, even though the canonical session root now names a newer generation.
    assert_eq!(
        fs::read_to_string(projected.join("generation")).unwrap(),
        format!("{first_generation}\n")
    );
    assert_eq!(
        fs::canonicalize(projected.join("wayland-0")).unwrap(),
        fs::canonicalize(first_wayland).unwrap()
    );
    assert_ne!(
        fs::canonicalize(projected.join("wayland-0")).unwrap(),
        fs::canonicalize(second_wayland).unwrap()
    );
}

#[test]
fn drm_and_protected_input_ownership_are_explicitly_denied() {
    let scratch = Scratch::new();
    let root = scratch.path("session");
    let publisher = SessionPublisher::new(&root);
    let endpoint = source_file(&scratch.path, "forbidden.sock");

    let result = SessionPublication::new("wayland-0", source_file(&scratch.path, "wayland.sock"))
        .with_capability("drm", endpoint.clone());
    assert!(matches!(
        result,
        Err(SessionContractError::DeniedCapability { .. })
    ));

    publisher.publish(&publication(&scratch.path)).unwrap();
    let result = publisher.project_app_root(&scratch.path("app-root"), &["protected-input"]);
    assert!(matches!(
        result,
        Err(SessionContractError::DeniedCapability { .. })
    ));
    assert!(!root.join("capabilities/drm").exists());
    assert!(!root.join("capabilities/protected-input").exists());
}

#[test]
fn canonical_name_is_stable() {
    assert_eq!(DEFAULT_SESSION_ROOT, "/run/pocketforge/session");
}
