use pf_framehost_wayland::WaylandHost;
use pf_ports::FrameHost;
use pf_scene::{Bounds, Node, NodeAction, NodeId, Role, Scene};

fn main() {
    let caption = Node::new(
        NodeId::new("fixture-caption").unwrap(),
        Role::Text,
        "Wayland fixture",
        Bounds::new(16.0, 16.0, 180.0, 64.0),
        "--state-rest-surface",
    );
    let node = Node::new(
        NodeId::new("fixture-card").unwrap(),
        Role::Button,
        "Wayland fixture",
        Bounds::new(16.0, 16.0, 180.0, 64.0),
        "--state-rest-surface",
    )
    .with_action(NodeAction::Activate)
    .with_children(vec![caption]);
    let scene = Scene::new(node, NodeId::new("fixture-card").unwrap()).unwrap();
    let mut host = WaylandHost::connect().expect("connect/configure");
    println!("CONNECT ok");
    println!("CONFIGURE {:?}", host.metrics());
    println!("PRESENT {:?}", host.present(&scene).expect("present"));
}
