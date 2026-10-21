use pf_input_broker::{pad_uinput_spec, pointer_uinput_spec};
use pf_input_ipc::PadDestination;

const EV_REL: u16 = 0x02;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL_HI_RES: u16 = 0x0b;
const REL_WHEEL_HI_RES: u16 = 0x0c;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;

#[test]
fn pointer_device_is_a_private_relative_mouse_with_high_resolution_scroll() {
    let spec = pointer_uinput_spec();
    assert_eq!(spec.name, "PocketForge Pointer");
    assert_eq!(
        spec.rel,
        [REL_X, REL_Y, REL_HWHEEL_HI_RES, REL_WHEEL_HI_RES]
    );
    assert_eq!(spec.keys, [BTN_LEFT, BTN_RIGHT]);
    assert!(spec.abs.is_empty());
    assert_eq!(EV_REL, 0x02, "Linux input ABI control");
}

#[test]
fn every_pad_destination_has_a_distinct_private_virtual_device_identity() {
    let base = pf_input_broker::UinputSpec {
        name: "descriptor-derived".into(),
        bus: 3,
        vendor: 4,
        product: 5,
        version: 6,
        keys: vec![0x130],
        rel: Vec::new(),
        abs: Vec::new(),
    };
    let destinations = [
        PadDestination::App,
        PadDestination::Keyboard,
        PadDestination::Menu,
        PadDestination::Shell,
    ];
    let specs: Vec<_> = destinations
        .into_iter()
        .map(|destination| pad_uinput_spec(&base, destination).unwrap())
        .collect();

    assert_eq!(
        specs
            .iter()
            .map(|spec| spec.name.as_str())
            .collect::<Vec<_>>(),
        [
            "PocketForge App Pad",
            "PocketForge Keyboard Pad",
            "PocketForge Menu Pad",
            "PocketForge Shell Pad",
        ]
    );
    let products: std::collections::BTreeSet<_> = specs.iter().map(|spec| spec.product).collect();
    assert_eq!(products.len(), specs.len());
    for spec in specs {
        assert_eq!(spec.bus, 0x06, "private devices use BUS_VIRTUAL");
        assert_eq!(spec.vendor, 0x5046, "PocketForge-owned virtual identity");
        assert_eq!(spec.keys, base.keys);
        assert_eq!(spec.abs.len(), base.abs.len());
    }
}
