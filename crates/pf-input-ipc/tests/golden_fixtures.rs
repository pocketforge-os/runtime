//! Golden producer fixtures: the canonical `register`, `state_snapshot` (three unmapped overlays)
//! and `focus` bodies exactly as this crate encodes them, so gamescope's C++ encoder can be
//! byte-checked against the same files. Regenerate with `PF_UPDATE_GOLDEN=1`.

use pf_input_ipc::compositor::{
    CompositorSnapshot, CompositorToRouter, FocusSnapshot, FocusedSurface, OverlaySnapshot,
    OverlayState, ProtectedPowerSnapshot, TextInputState,
};
use pf_input_ipc::{decode, encode, ContentPurpose, OverlayRole};
use std::path::PathBuf;

fn text_input() -> TextInputState {
    TextInputState {
        active: false,
        purpose: ContentPurpose::Normal,
    }
}

fn cases() -> Vec<(&'static str, CompositorToRouter)> {
    let overlays = [OverlayRole::Keyboard, OverlayRole::Menu, OverlayRole::Toast]
        .into_iter()
        .map(|role| OverlaySnapshot {
            role,
            state: OverlayState::Unmapped,
            rect: None,
        })
        .collect();
    vec![
        ("register.json", CompositorToRouter::Register),
        (
            "state_snapshot.json",
            CompositorToRouter::StateSnapshot {
                snapshot: CompositorSnapshot {
                    epoch: 1,
                    focus: FocusSnapshot {
                        surface: FocusedSurface::None,
                        text_input: text_input(),
                        secure_entry: false,
                    },
                    overlays,
                    power: ProtectedPowerSnapshot {
                        pressed: false,
                        last_sequence: 0,
                    },
                },
            },
        ),
        (
            "focus.json",
            CompositorToRouter::Focus {
                epoch: 1,
                surface: FocusedSurface::App {
                    app_id: "org.example.game".into(),
                },
                text_input: text_input(),
                secure_entry: false,
            },
        ),
    ]
}

#[test]
fn golden_fixtures_round_trip_byte_exact() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    for (name, message) in cases() {
        let path = dir.join(name);
        let encoded = encode(&message);
        if std::env::var_os("PF_UPDATE_GOLDEN").is_some() {
            std::fs::write(&path, &encoded).unwrap();
        }
        let golden = std::fs::read(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(golden, encoded, "{name}: golden bytes differ from encode()");
        let decoded: CompositorToRouter = decode(&golden).unwrap();
        assert_eq!(decoded, message, "{name}: decode mismatch");
    }
}
