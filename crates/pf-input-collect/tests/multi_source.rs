//! MULTI-SOURCE routing (tsp-bwrg.16): a control whose descriptor `source` names a NON-PRIMARY
//! evdev node is captured from THAT node — and the SAME plan against a single-source collector
//! misses it entirely. That miss is the exact structural gap this bead exists to close: the a133's
//! VOL± live on the `sunxi-keyboard` LRADC node (event0), the guided collector read only the
//! `TRIMUI Player1` gamepad node (event2), so no plan change could ever have captured them.
//!
//! Proven-to-fail (guards-must-be-shown-to-fail): `single_source_build_misses_the_nonprimary_control`
//! reproduces the PRE-CHANGE behaviour — a single node, `set_active_source` a no-op — and asserts the
//! control is ABSENT; `multi_source_captures_the_nonprimary_control` asserts the multi-node router
//! captures it. Same plan, same events; the only difference is the source topology.

use pf_input_collect::collect::{self, DeviceMeta, Recorded, RunConfig};
use pf_input_collect::plan::{ControlSpec, Kind};
use pf_input_collect::source::{Identity, RawEvent, ScriptedSource};
use pf_input_collect::{Collector, EventSource, MultiSource};

const EV_KEY: u16 = 0x01;
const BTN_SOUTH: u16 = 0x130;
const KEY_VOLUMEUP: u16 = 0x73; // 115 — the LRADC/sunxi-keyboard code the gamepad node never carries

fn key(code: u16, v: i32) -> RawEvent {
    RawEvent::new(EV_KEY, code, v)
}

fn gamepad_ident() -> Identity {
    Identity { name: "TRIMUI Player1".into(), bus: 3, vid: 0x045e, pid: 0x028e, version: 0x0110 }
}
fn kbd_ident() -> Identity {
    // The LRADC node advertises no vid/pid of interest; identity() is only ever taken from PRIMARY.
    Identity { name: "sunxi-keyboard".into(), bus: 0x19, vid: 0x0001, pid: 0x0001, version: 0 }
}

fn cfg() -> RunConfig {
    RunConfig {
        quiet_polls: 2,
        idle_skip_polls: 4, // an optional control with no activity skips after a few quiet polls
        max_polls: 2000,
        control_timeout: std::time::Duration::from_secs(5),
        ..RunConfig::default()
    }
}

fn meta() -> DeviceMeta {
    DeviceMeta { id: "a133".into(), manufacturer: "TrimUI".into(), model: "Smart Pro".into() }
}

/// Two controls: `south` on the primary gamepad node, `vol_up` pinned to the `sunxi-keyboard` node.
/// `vol_up` is optional so the single-source arm SKIPS it (row omitted) rather than erroring —
/// which is exactly how a real optional system control would behave when its node is never read.
fn plan() -> Vec<ControlSpec> {
    vec![
        ControlSpec::new("south", Kind::Button, "press south", false),
        ControlSpec::new("vol_up", Kind::Button, "press VOL+", true).on_source("sunxi-keyboard"),
    ]
}

fn pad_with_south() -> ScriptedSource {
    let mut pad = ScriptedSource::new(gamepad_ident());
    pad.push_batch(vec![key(BTN_SOUTH, 1), key(BTN_SOUTH, 0)]);
    pad.push_batch(vec![]);
    pad.push_batch(vec![]);
    pad
}

#[test]
fn multi_source_captures_the_nonprimary_control() {
    let pad = pad_with_south();

    // VOL+ fires ONLY on the sunxi-keyboard node — never on the pad. If the engine reads the wrong
    // node, this press is invisible and vol_up skips.
    let mut kbd = ScriptedSource::new(kbd_ident());
    kbd.push_batch(vec![key(KEY_VOLUMEUP, 1), key(KEY_VOLUMEUP, 0)]);
    kbd.push_batch(vec![]);
    kbd.push_batch(vec![]);

    let mut src = MultiSource::new("TRIMUI Player1", Box::new(pad));
    src.add_source("sunxi-keyboard", Box::new(kbd));

    let mut c = Collector::new(plan());
    let mut log = Vec::new();
    let caps = collect::run(&mut c, &mut src, &meta(), &cfg(), &mut log).expect("run ok");

    assert_eq!(
        c.recorded("south"),
        Some(&Recorded::Button { code: BTN_SOUTH }),
        "south must be captured from the primary gamepad node"
    );
    // THE POINT: a control on a NON-PRIMARY node was captured, from that node.
    let vol = caps
        .inputs
        .iter()
        .find(|i| i.id == "vol_up")
        .expect("vol_up must be captured from the sunxi-keyboard node (multi-source routing)");
    assert_eq!(vol.code, "KEY_VOLUMEUP");
    // identity() is ALWAYS the primary node's, never the keyboard node's.
    assert_eq!(caps.identity.evdev_name, "TRIMUI Player1");
}

#[test]
fn single_source_build_misses_the_nonprimary_control() {
    // PRE-CHANGE reproduction: ONE node (the gamepad pad). `set_active_source` is a no-op on a
    // plain ScriptedSource, so vol_up's prompt reads the pad — where VOL+ never appears.
    let mut src = pad_with_south();

    let mut c = Collector::new(plan());
    let mut log = Vec::new();
    let caps = collect::run(&mut c, &mut src, &meta(), &cfg(), &mut log).expect("run ok");

    assert_eq!(
        c.recorded("south"),
        Some(&Recorded::Button { code: BTN_SOUTH }),
        "south is still captured (it IS on the only node present)"
    );
    // THE MISS: vol_up is structurally uncapturable against a single node — absent from the output.
    assert!(
        caps.inputs.iter().all(|i| i.id != "vol_up"),
        "single-source build must MISS vol_up (it lives on a node this run never reads) — \
         got inputs: {:?}",
        caps.inputs.iter().map(|i| &i.id).collect::<Vec<_>>()
    );
}

#[test]
fn unknown_source_node_falls_back_to_primary_and_flags_the_miss() {
    // A descriptor referencing a node the run never opened must NOT silently misread the primary as
    // if it were that node: the router falls back to primary and RECORDS the miss so a caller can
    // surface "descriptor names a node this run did not open" instead of a phantom capture.
    let pad = pad_with_south();
    let mut src = MultiSource::new("TRIMUI Player1", Box::new(pad));

    src.set_active_source(Some("audiocodec sunxi Audio Jack")); // never registered
    assert!(src.last_route_missed(), "an unregistered node must be flagged as a routing miss");

    src.set_active_source(Some("TRIMUI Player1"));
    assert!(!src.last_route_missed(), "the primary node is registered — no miss");

    src.set_active_source(None);
    assert!(!src.last_route_missed(), "None routes to primary — no miss");
}
