use pf_scene::{Bounds, Node, NodeAction, NodeId, Role, Scene, SceneError, StateTransition};

fn id(value: &str) -> NodeId {
    NodeId::new(value).expect("test id is valid")
}

#[test]
fn menu_roles_and_value_text_are_semantic_state() {
    let roles = [
        Role::Choice,
        Role::TextField,
        Role::Progress,
        Role::Tab,
        Role::Link,
        Role::Separator,
    ];
    assert_eq!(roles.len(), 6);

    let mut node = Node::new(
        id("choice"),
        Role::Choice,
        "Scaling",
        Bounds::new(0.0, 0.0, 100.0, 48.0),
        "menu-row",
    )
    .with_value_text("Integer");
    assert_eq!(node.state.value_text.as_deref(), Some("Integer"));

    let mut scene = Scene::new(node.clone().with_action(NodeAction::Activate), id("choice"))
        .expect("positive control: ordinary scene is valid");
    scene
        .transition_state(
            &id("choice"),
            StateTransition::ValueText(Some("Fit".to_string())),
        )
        .expect("value text transition succeeds");
    node = scene.root().clone();
    assert_eq!(node.state.value_text.as_deref(), Some("Fit"));
    scene
        .transition_state(&id("choice"), StateTransition::Pending(true))
        .expect("pending state transition succeeds");
    assert!(scene.root().state.pending);
}

#[test]
fn external_menu_focus_preserves_disabled_rows_and_rejects_missing_ids() {
    let mut disabled = Node::new(
        id("disabled"),
        Role::Choice,
        "Reader view",
        Bounds::new(0.0, 0.0, 100.0, 48.0),
        "menu-row",
    )
    .with_action(NodeAction::Activate);
    disabled.state.disabled = true;
    let enabled = Node::new(
        id("enabled"),
        Role::Button,
        "Reload",
        Bounds::new(0.0, 48.0, 100.0, 48.0),
        "menu-row",
    )
    .with_action(NodeAction::Activate);
    let root = Node::new(
        id("root"),
        Role::Group,
        "Menu",
        Bounds::new(0.0, 0.0, 100.0, 96.0),
        "menu",
    )
    .with_children(vec![disabled, enabled]);

    let scene = Scene::new_with_external_focus(root.clone(), id("enabled"), Some(id("disabled")))
        .expect("engine-owned focus may rest on a disabled menu row");
    assert_eq!(scene.focused(), Some(&id("disabled")));
    assert!(scene.focused_node().expect("focused node").state.disabled);

    assert_eq!(
        Scene::new_with_external_focus(root, id("enabled"), Some(id("missing"))),
        Err(SceneError::ExternalFocusMissing(id("missing")))
    );
}

#[test]
fn external_focus_updates_are_exact_and_failure_atomic() {
    let first = Node::new(
        id("first"),
        Role::Button,
        "First",
        Bounds::new(0.0, 0.0, 100.0, 48.0),
        "menu-row",
    )
    .with_action(NodeAction::Activate);
    let mut second = Node::new(
        id("second"),
        Role::Button,
        "Second",
        Bounds::new(0.0, 48.0, 100.0, 48.0),
        "menu-row",
    )
    .with_action(NodeAction::Activate);
    second.state.disabled = true;
    let root = Node::new(
        id("root"),
        Role::Group,
        "Menu",
        Bounds::new(0.0, 0.0, 100.0, 96.0),
        "menu",
    )
    .with_children(vec![first, second]);
    let mut scene = Scene::new_with_external_focus(root, id("first"), Some(id("first"))).unwrap();

    scene.set_external_focus(Some(id("second"))).unwrap();
    assert_eq!(scene.focused(), Some(&id("second")));
    assert!(scene.focused_node().unwrap().state.disabled);

    assert_eq!(
        scene.set_external_focus(Some(id("missing"))),
        Err(SceneError::ExternalFocusMissing(id("missing")))
    );
    assert_eq!(scene.focused(), Some(&id("second")));

    scene.set_external_focus(None).unwrap();
    assert_eq!(scene.focused(), None);
}
