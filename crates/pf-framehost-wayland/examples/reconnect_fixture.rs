use pf_framehost_wayland::WaylandHost;
use pf_ports::{FrameHost, PresentFailure};
use pf_scene::{Bounds, Node, NodeAction, NodeId, Role, Scene};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn start(runtime: &Path, socket: &str) -> Child {
    let _ = std::fs::remove_file(runtime.join(socket));
    let child = Command::new("weston")
        .args([
            "--backend=headless-backend.so",
            &format!("--socket={socket}"),
            "--idle-time=0",
        ])
        .env("XDG_RUNTIME_DIR", runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start weston");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !runtime.join(socket).exists() {
        assert!(Instant::now() < deadline, "Weston socket timeout");
        thread::sleep(Duration::from_millis(20));
    }
    child
}

fn fixture() -> Scene {
    let node = Node::new(
        NodeId::new("fixture-card").unwrap(),
        Role::Button,
        // Keep this identical to pf-framehost's offscreen/fbdev trait-parity fixture.
        "続ける",
        Bounds::new(7.0, 9.0, 120.0, 51.0),
        "card",
    )
    .with_action(NodeAction::Activate);
    Scene::new(node, NodeId::new("fixture-card").unwrap()).unwrap()
}

fn main() {
    let runtime = tempfile::tempdir().unwrap();
    std::fs::set_permissions(
        runtime.path(),
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    let socket = "wayland-pocketforge-reconnect";
    std::env::set_var("XDG_RUNTIME_DIR", runtime.path());
    std::env::set_var("WAYLAND_DISPLAY", socket);
    let scene = fixture();

    let mut compositor = start(runtime.path(), socket);
    let mut host = WaylandHost::connect().expect("initial connect/configure");
    println!("CONNECT ok");
    println!("CONFIGURE {:?}", host.metrics());
    println!(
        "PRESENT {:?}",
        host.present(&scene).expect("initial present")
    );

    compositor.kill().unwrap();
    compositor.wait().unwrap();
    assert_eq!(host.present(&scene), Err(PresentFailure::SurfaceLost));
    println!("DISCONNECT SurfaceLost");

    let mut compositor = start(runtime.path(), socket);
    host.reconnect().expect("reconnect/configure");
    println!("RECONNECT ok");
    println!(
        "PRESENT {:?}",
        host.present(&scene).expect("present after reconnect")
    );
    compositor.kill().unwrap();
    compositor.wait().unwrap();
}
