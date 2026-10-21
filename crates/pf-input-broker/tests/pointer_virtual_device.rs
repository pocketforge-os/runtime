use pf_input_broker::pointer_uinput_spec;

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
    assert_eq!(spec.rel, [REL_X, REL_Y, REL_HWHEEL_HI_RES, REL_WHEEL_HI_RES]);
    assert_eq!(spec.keys, [BTN_LEFT, BTN_RIGHT]);
    assert!(spec.abs.is_empty());
    assert_eq!(EV_REL, 0x02, "Linux input ABI control");
}
