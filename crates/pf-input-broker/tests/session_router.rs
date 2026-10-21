use std::time::Duration;

use pf_input_broker::router::{
    AppMenuItem, AppMenuKind, AppSectionStore, Axis, ButtonEvent, Destination, PointerEngine,
    PointerFrame, RouteEffect, SessionRouter,
};
use pf_input_broker::safe_return::GUIDE_CODE;

const SOUTH: u16 = 0x130;

#[test]
fn menu_is_consumed_before_every_destination_and_after_resync() {
    for destination in Destination::ALL {
        let mut router = SessionRouter::new(destination);
        assert_eq!(
            router.route_button(ButtonEvent::new(GUIDE_CODE, true), Duration::ZERO),
            vec![RouteEffect::MenuPress],
            "destination={destination:?}"
        );
        assert_eq!(
            router.route_button(ButtonEvent::new(SOUTH, true), Duration::ZERO),
            vec![RouteEffect::Button {
                destination,
                code: SOUTH,
                pressed: true,
            }],
            "ordinary input is the negative control for {destination:?}"
        );

        let mut dropped = SessionRouter::new(destination);
        assert_eq!(
            dropped.resynchronize([(GUIDE_CODE, true), (SOUTH, true)], Duration::ZERO),
            vec![
                RouteEffect::MenuPress,
                RouteEffect::Button {
                    destination,
                    code: SOUTH,
                    pressed: true,
                },
            ],
            "destination={destination:?}"
        );
    }
}

#[test]
fn every_switch_releases_the_old_destination_before_the_new_one() {
    let mut router = SessionRouter::new(Destination::App);
    router.route_button(ButtonEvent::new(SOUTH, true), Duration::ZERO);
    assert_eq!(
        router.switch_to(Destination::Pointer),
        vec![
            RouteEffect::Button {
                destination: Destination::App,
                code: SOUTH,
                pressed: false,
            },
            RouteEffect::Sync {
                destination: Destination::App,
            },
            RouteEffect::DestinationChanged(Destination::Pointer),
        ]
    );
    assert!(router.switch_to(Destination::Pointer).is_empty());
}

#[test]
fn menu_deadlines_and_two_second_hold_use_the_required_return_paths() {
    let mut no_ack = SessionRouter::new(Destination::App);
    no_ack.route_button(ButtonEvent::new(GUIDE_CODE, true), Duration::ZERO);
    assert_eq!(
        no_ack.tick(Duration::from_millis(251)),
        vec![RouteEffect::SafeReturn]
    );

    let mut no_shown = SessionRouter::new(Destination::App);
    no_shown.route_button(ButtonEvent::new(GUIDE_CODE, true), Duration::ZERO);
    assert!(no_shown.menu_ack(Duration::from_millis(250)).is_empty());
    assert_eq!(
        no_shown.tick(Duration::from_millis(751)),
        vec![RouteEffect::DismissMenu, RouteEffect::SafeReturn]
    );

    let mut silent = SessionRouter::new(Destination::App);
    silent.route_button(ButtonEvent::new(GUIDE_CODE, true), Duration::ZERO);
    silent.menu_ack(Duration::from_millis(1));
    silent.menu_shown(Duration::from_millis(2));
    assert_eq!(
        silent.tick(Duration::from_millis(1_003)),
        vec![
            RouteEffect::DismissMenu,
            RouteEffect::DestinationChanged(Destination::App),
        ]
    );

    let mut held = SessionRouter::new(Destination::Pointer);
    held.route_button(ButtonEvent::new(GUIDE_CODE, true), Duration::ZERO);
    assert!(held.tick(Duration::from_millis(1_999)).is_empty());
    assert_eq!(
        held.tick(Duration::from_secs(2)),
        vec![RouteEffect::DismissMenu, RouteEffect::SafeReturn]
    );
}

#[test]
fn pointer_curve_is_radial_squared_crosses_1280px_per_second_and_expires_assist() {
    let mut pointer = PointerEngine::default();
    assert_eq!(pointer.frame(0.1, 0.0, false, Duration::ZERO), PointerFrame::ZERO);
    let full = pointer.frame(1.0, 0.0, false, Duration::ZERO);
    assert_eq!(full.dx_per_second, 1280.0);
    assert_eq!(full.dy_per_second, 0.0);
    let precise = pointer.frame(1.0, 0.0, true, Duration::ZERO);
    assert_eq!(precise.dx_per_second, 448.0);

    pointer.set_assist(0.5, Duration::ZERO).unwrap();
    assert!(pointer.frame(0.3, 0.0, false, Duration::from_millis(120)).dx_per_second <
        PointerEngine::default().frame(0.3, 0.0, false, Duration::from_millis(120)).dx_per_second);
    assert_eq!(
        pointer.frame(0.3, 0.0, false, Duration::from_millis(121)),
        PointerEngine::default().frame(0.3, 0.0, false, Duration::from_millis(121))
    );

    assert_eq!(PointerEngine::scroll_axis(12.0, 11.0), Axis::Vertical(12.0));
    assert_eq!(PointerEngine::scroll_axis(11.0, 12.0), Axis::Horizontal(12.0));
    assert_eq!(PointerEngine::scroll_axis(0.0, 0.0), Axis::None);
}

#[test]
fn app_section_limits_and_spoofing_labels_fail_closed_without_mutation() {
    let mut store = AppSectionStore::default();
    let valid = AppMenuItem::new("save", "Save game", AppMenuKind::Action);
    store.update("org.example.game", vec![valid], 100, Duration::ZERO).unwrap();
    let before = store.items("org.example.game").unwrap().to_vec();

    for label in ["Close", "Return to game", "P\u{202e}ower", "Ρower"] {
        let bad = AppMenuItem::new("bad", label, AppMenuKind::Action);
        assert!(store
            .update("org.example.game", vec![bad], 100, Duration::from_secs(1))
            .is_err(),
            "label={label:?}");
        assert_eq!(store.items("org.example.game").unwrap(), before.as_slice());
    }

    let too_many = (0..9)
        .map(|i| AppMenuItem::new(format!("item-{i}"), format!("Item {i}"), AppMenuKind::Action))
        .collect();
    assert!(store.update("org.example.game", too_many, 100, Duration::from_secs(2)).is_err());
    assert!(store.update("org.example.game", vec![], 4097, Duration::from_secs(3)).is_err());
}
