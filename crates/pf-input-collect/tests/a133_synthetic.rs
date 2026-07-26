//! Acceptance test for the guided-collection engine (`tsp-bwrg.2`).
//!
//! Feeds the engine a SYNTHETIC a133-shaped decoded-evdev stream that mirrors the REAL decoder
//! output (`pf-input-decode`, `tsp-ozbp.9`) — no kernel, no `/dev/uinput`, CI-safe — runs the full
//! prompt sequence headlessly via `collect::run`, and asserts:
//!   1. the emitted `[[inputs]]` rows match the a133 ground-truth code map — the `tsp-ozbp.2` UART
//!      decode + the owner-verified frozen evdev baseline (17/17), i.e. the DECODER's real output.
//!      (Note: `platform/devices/a133/capabilities.toml` still models L2/R2 as `ABS_Z`/`ABS_RZ`
//!      analog axes — stale vs the decoder's button output; that descriptor correction is
//!      tsp-e1b-coord's lane, see the tsp-bwrg.6 validation report.)
//!   2. the L2/R2 triggers are BINARY triggers realized as BUTTONS (`BTN_TL2`/`BTN_TR2`,
//!      `semantics="binary"`) — the a133 MCU reports them as bits in the button bitmask;
//!   3. stick axis ranges + the derived SDL GUID + identity match;
//!   4. (when a `platform` checkout is discoverable) the emitted candidate PASSES the real
//!      `platform/core/caps.py validate` — in a self-contained temp tree, so it neither needs
//!      `platform` beside `runtime` in CI nor mutates any checkout.

use std::collections::BTreeMap;

use pf_input_collect::collect::{self, DeviceMeta, RunConfig};
use pf_input_collect::plan::default_gamepad_plan;
use pf_input_collect::source::{AbsInfo, Identity, RawEvent, ScriptedSource};
use pf_input_collect::Collector;

// --- evdev code constants (kernel ABI) ---
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const BTN_A: u16 = 0x130;
const BTN_B: u16 = 0x131;
const BTN_X: u16 = 0x133;
const BTN_Y: u16 = 0x134;
const BTN_SELECT: u16 = 0x13a;
const BTN_START: u16 = 0x13b;
const BTN_MODE: u16 = 0x13c;
const BTN_TL: u16 = 0x136;
const BTN_TR: u16 = 0x137;
const BTN_TL2: u16 = 0x138; // L2 — the a133 left trigger, emitted as a BUTTON by the decoder
const BTN_TR2: u16 = 0x139; // R2 — the a133 right trigger, emitted as a BUTTON by the decoder
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_Z: u16 = 0x02;
const ABS_RX: u16 = 0x03;
const ABS_RY: u16 = 0x04;
const ABS_RZ: u16 = 0x05;
const ABS_HAT0X: u16 = 0x10;
const ABS_HAT0Y: u16 = 0x11;

fn key(code: u16, val: i32) -> RawEvent {
    RawEvent::new(EV_KEY, code, val)
}
fn abs(code: u16, val: i32) -> RawEvent {
    RawEvent::new(EV_ABS, code, val)
}

const STICK: AbsInfo = AbsInfo { min: -32768, max: 32767, fuzz: 16, flat: 128, resolution: 0 };
const TRIG: AbsInfo = AbsInfo { min: 0, max: 255, fuzz: 0, flat: 0, resolution: 0 };

/// Build the synthetic a133 source. Batches are queued in PLAN ORDER; empty batches act as the
/// inter-control "quiet" separators the sweep-settle / optional-skip heuristics key on.
fn a133_source() -> ScriptedSource {
    let ident = Identity {
        name: "TRIMUI Player1".to_string(),
        bus: 0x0003, // BUS_USB
        vid: 0x045e, // Microsoft
        pid: 0x028e, // Xbox 360 wired
        version: 0x0110,
    };
    // The real a133 decoder (pf-input-decode, tsp-ozbp.9) advertises ONLY these axes — the two
    // sticks + the dpad hat. It does NOT advertise ABS_Z/ABS_RZ: L2/R2 come over the UART as
    // BINARY BITS in the button bitmask and are emitted as BTN_TL2/BTN_TR2 buttons, never axes.
    let mut s = ScriptedSource::new(ident)
        .with_abs(ABS_X, STICK)
        .with_abs(ABS_Y, STICK)
        .with_abs(ABS_RX, STICK)
        .with_abs(ABS_RY, STICK);

    // Buttons complete on key-down, so a single batch each; no separator needed.
    for code in [BTN_A, BTN_B, BTN_X, BTN_Y, BTN_SELECT, BTN_START, BTN_MODE, BTN_TL, BTN_TR] {
        s.push_batch(vec![key(code, 1), key(code, 0)]);
    }

    // dpad hat — both axes actuated; then two empties to settle (quiet_polls=2).
    s.push_batch(vec![
        abs(ABS_HAT0X, -1), abs(ABS_HAT0X, 0), abs(ABS_HAT0X, 1), abs(ABS_HAT0X, 0),
        abs(ABS_HAT0Y, -1), abs(ABS_HAT0Y, 0), abs(ABS_HAT0Y, 1), abs(ABS_HAT0Y, 0),
    ]);
    s.push_batch(vec![]);
    s.push_batch(vec![]);

    // left stick sweep — full swing both axes; settle.
    s.push_batch(vec![
        abs(ABS_X, -32768), abs(ABS_X, 0), abs(ABS_X, 32767), abs(ABS_X, 0),
        abs(ABS_Y, -32768), abs(ABS_Y, 0), abs(ABS_Y, 32767), abs(ABS_Y, 0),
    ]);
    s.push_batch(vec![]);
    s.push_batch(vec![]);

    // right stick sweep; settle.
    s.push_batch(vec![
        abs(ABS_RX, -32768), abs(ABS_RX, 32767), abs(ABS_RX, 0),
        abs(ABS_RY, -32768), abs(ABS_RY, 32767), abs(ABS_RY, 0),
    ]);
    s.push_batch(vec![]);
    s.push_batch(vec![]);

    // l3 + r3 stick-clicks — NOT on the base unit: no activity → optional-skip (idle_skip_polls=2).
    for _ in 0..2 {
        s.push_batch(vec![]);
    }
    for _ in 0..2 {
        s.push_batch(vec![]);
    }

    // ltrig — a BINARY trigger realized as a BUTTON: the a133 L2 fires BTN_TL2 (no analog axis).
    // Completes on key-down, like the face buttons.
    s.push_batch(vec![key(BTN_TL2, 1), key(BTN_TL2, 0)]);

    // rtrig — R2 fires BTN_TR2.
    s.push_batch(vec![key(BTN_TR2, 1), key(BTN_TR2, 0)]);

    s
}

fn test_cfg() -> RunConfig {
    RunConfig { quiet_polls: 2, idle_skip_polls: 2, max_polls: 60, ..RunConfig::default() }
}

fn run_a133() -> (pf_input_collect::Capabilities, String) {
    let mut src = a133_source();
    let mut collector = Collector::new(default_gamepad_plan());
    let meta = DeviceMeta {
        id: "a133".to_string(),
        manufacturer: "TrimUI".to_string(),
        model: "Smart Pro".to_string(),
    };
    let mut log = Vec::new();
    let caps = collect::run(&mut collector, &mut src, &meta, &test_cfg(), &mut log)
        .expect("collection run should succeed on the synthetic a133 stream");
    (caps, String::from_utf8(log).unwrap())
}

#[test]
fn emitted_inputs_match_the_a133_ground_truth_code_map() {
    let (caps, transcript) = run_a133();
    // Print the transcript so `cargo test -- --nocapture` yields the PR-attachable run log.
    println!("\n--- guided-collection run transcript (synthetic a133) ---\n{transcript}");
    println!("--- emitted candidate capabilities.toml ---\n{}", caps.to_toml());

    // The a133 ground-truth map (id -> (kind, ev_type, code[, semantics])) from
    // platform/devices/a133/capabilities.toml (the tsp-ozbp.2 decode).
    let expected: BTreeMap<&str, (&str, &str, &str, Option<&str>)> = BTreeMap::from([
        ("south", ("button", "EV_KEY", "BTN_A", None)),
        ("east", ("button", "EV_KEY", "BTN_B", None)),
        ("west", ("button", "EV_KEY", "BTN_X", None)),
        ("north", ("button", "EV_KEY", "BTN_Y", None)),
        ("select", ("button", "EV_KEY", "BTN_SELECT", None)),
        ("start", ("button", "EV_KEY", "BTN_START", None)),
        ("guide", ("button", "EV_KEY", "BTN_MODE", None)),
        ("l1", ("button", "EV_KEY", "BTN_TL", None)),
        ("r1", ("button", "EV_KEY", "BTN_TR", None)),
        ("dpad", ("hat", "EV_ABS", "ABS_HAT0X,ABS_HAT0Y", None)),
        ("lstick", ("stick", "EV_ABS", "ABS_X,ABS_Y", None)),
        ("rstick", ("stick", "EV_ABS", "ABS_RX,ABS_RY", None)),
        // L2/R2: binary triggers realized as buttons — kind=trigger (intent), EV_KEY BTN_TL2/TR2
        // (the decoder's real output), semantics=binary. This is the owner-verified reality
        // (tsp-ozbp.2 + frozen evdev baseline 17/17), NOT the stale ABS_Z/RZ analog model.
        ("ltrig", ("trigger", "EV_KEY", "BTN_TL2", Some("binary"))),
        ("rtrig", ("trigger", "EV_KEY", "BTN_TR2", Some("binary"))),
    ]);

    let got: BTreeMap<String, (String, String, String, Option<String>)> = caps
        .inputs
        .iter()
        .map(|i| {
            (
                i.id.clone(),
                (i.kind.clone(), i.ev_type.clone(), i.code.clone(), i.semantics.clone()),
            )
        })
        .collect();

    // Every expected control present with the exact kind/ev_type/code/semantics.
    for (id, (kind, ev, code, sem)) in &expected {
        let g = got.get(*id).unwrap_or_else(|| panic!("emitted candidate missing input '{id}'"));
        assert_eq!(&g.0, kind, "{id}: kind");
        assert_eq!(&g.1, ev, "{id}: ev_type");
        assert_eq!(&g.2, code, "{id}: code");
        assert_eq!(g.3.as_deref(), *sem, "{id}: semantics");
    }
    // No spurious rows (l3/r3 were correctly omitted, not fabricated).
    assert_eq!(got.len(), expected.len(), "row count differs: got ids {:?}", got.keys().collect::<Vec<_>>());
    assert!(!got.contains_key("l3"), "absent stick-click l3 must be omitted, not fabricated");
    assert!(!got.contains_key("r3"), "absent stick-click r3 must be omitted, not fabricated");
}

#[test]
fn identity_sdl_guid_and_stick_ranges_are_correct() {
    let (caps, _) = run_a133();
    // Identity + derived SDL GUID match the shipped a133 exactly.
    assert_eq!(caps.identity.id, "a133");
    assert_eq!(caps.identity.evdev_name, "TRIMUI Player1");
    assert_eq!(caps.identity.vid.as_deref(), Some("045e"));
    assert_eq!(caps.identity.pid.as_deref(), Some("028e"));
    assert_eq!(caps.identity.sdl_guid, "030000005e0400008e02000010010000");
    assert_eq!(caps.accept_default.as_deref(), Some("south"));

    // Stick ranges carry the declared EVIOCGABS absinfo.
    let lstick = caps.inputs.iter().find(|i| i.id == "lstick").unwrap();
    let x = lstick.x.expect("lstick has x axis");
    assert_eq!((x.min, x.max, x.fuzz, x.flat), (-32768, 32767, 16, 128));
    assert!(lstick.y.is_some());

    // The a133 trigger is a BUTTON on the wire (BTN_TL2), so it carries NO analog range — but it
    // is still marked semantics=binary (a binary trigger realized as a button).
    let ltrig = caps.inputs.iter().find(|i| i.id == "ltrig").unwrap();
    assert_eq!(ltrig.ev_type, "EV_KEY");
    assert_eq!(ltrig.code, "BTN_TL2");
    assert!(ltrig.range.is_none(), "a button-realized trigger has no analog range");
    assert_eq!(ltrig.semantics.as_deref(), Some("binary"));
}

#[test]
fn analog_trigger_is_classified_analog() {
    // A trigger that DOES report intermediate travel must classify analog — the discriminator
    // is observed behaviour, not the declared range (identical to the binary case).
    let ident = Identity {
        name: "Generic Pad".to_string(),
        bus: 3,
        vid: 0x1234,
        pid: 0x5678,
        version: 0x0001,
    };
    let mut src = ScriptedSource::new(ident).with_abs(ABS_Z, TRIG);
    let mut collector = Collector::new(vec![pf_input_collect::plan::ControlSpec {
        id: "ltrig".into(),
        kind: pf_input_collect::plan::Kind::Trigger,
        prompt: "squeeze".into(),
        optional: false,
    }]);
    // Smooth ramp 0 -> 255 with real intermediate values.
    src.push_batch((0..=255).step_by(5).map(|v| abs(ABS_Z, v)).collect());
    src.push_batch(vec![]);
    src.push_batch(vec![]);
    let meta = DeviceMeta { id: "gen".into(), manufacturer: "G".into(), model: "Pad".into() };
    let mut log = Vec::new();
    let caps = collect::run(&mut collector, &mut src, &meta, &test_cfg(), &mut log).unwrap();
    assert_eq!(caps.inputs[0].semantics.as_deref(), Some("analog"));
}

// -------------------------------------------------------------------------------------------
// Optional: validate the emitted candidate with the REAL platform/core/caps.py, when a platform
// checkout is discoverable. Self-contained (copies caps.py + schema into a temp tree), so it
// never needs `platform` beside `runtime` in CI and never mutates a checkout. Skips gracefully.
// -------------------------------------------------------------------------------------------

fn discover_platform() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("PF_PLATFORM_DIR") {
        let pb = std::path::PathBuf::from(p);
        if pb.join("core/caps.py").is_file() {
            return Some(pb);
        }
    }
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")); // .../runtime/crates/pf-input-collect
    for rel in ["../../../platform", "../../platform"] {
        let pb = manifest.join(rel);
        if pb.join("core/caps.py").is_file() {
            return Some(pb.canonicalize().unwrap_or(pb));
        }
    }
    None
}

#[test]
fn candidate_passes_real_caps_py_validate_when_platform_available() {
    let platform = match discover_platform() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no platform checkout found (set PF_PLATFORM_DIR to run the real caps.py validation)");
            return;
        }
    };
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        eprintln!("SKIP: python3 not available");
        return;
    }

    let (caps, _) = run_a133();
    let toml = caps.to_toml();

    // Build a self-contained temp platform-shaped tree: copy caps.py + schema, drop in
    // devices/a133/{capabilities.toml, minimal profile.toml}. caps.py derives ROOT from its own
    // path, so ROOT is the temp dir.
    let tmp = std::env::temp_dir().join(format!("pf-input-collect-caps-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("core")).unwrap();
    std::fs::create_dir_all(tmp.join("schemas")).unwrap();
    std::fs::create_dir_all(tmp.join("devices/a133")).unwrap();
    std::fs::copy(platform.join("core/caps.py"), tmp.join("core/caps.py")).unwrap();
    std::fs::copy(
        platform.join("schemas/capabilities.schema.json"),
        tmp.join("schemas/capabilities.schema.json"),
    )
    .unwrap();
    std::fs::write(tmp.join("devices/a133/capabilities.toml"), &toml).unwrap();
    // Minimal sibling profile.toml so the device.id join passes (a real device grows one later).
    std::fs::write(tmp.join("devices/a133/profile.toml"), "[device]\nid = \"a133\"\n").unwrap();

    let out = std::process::Command::new("python3")
        .arg(tmp.join("core/caps.py"))
        .arg("validate")
        .arg("a133")
        .output()
        .expect("run caps.py");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(&tmp);

    println!("--- caps.py validate a133 ---\n{stdout}{stderr}");
    assert!(
        out.status.success(),
        "caps.py validate FAILED (exit {:?}):\n{stdout}{stderr}",
        out.status.code()
    );
    assert!(stdout.contains("OK    a133"), "expected 'OK a133' from caps.py, got:\n{stdout}");
}
