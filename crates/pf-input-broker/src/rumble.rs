//! App Pad rumble forwarding: the App Pad uinput device advertises `EV_FF`/`FF_RUMBLE` only when
//! a vibrator actuator exists; upload/play/erase requests from the app are forwarded to that
//! actuator through an [`FfSink`], and every effect is stopped on focus loss, close and
//! safe-return so rumble can never outlive the app's claim on the pad.

use std::collections::BTreeSet;
use std::io;

/// The actuator side. Real implementations drive the vibrator evdev node; tests use a fake.
pub trait FfSink {
    fn upload(&mut self, id: i16, strong: u16, weak: u16, length_ms: u16) -> io::Result<()>;
    fn play(&mut self, id: i16, count: i32) -> io::Result<()>;
    fn stop(&mut self, id: i16) -> io::Result<()>;
    fn erase(&mut self, id: i16) -> io::Result<()>;
}

/// One force-feedback request read from the App Pad uinput device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfRequest {
    Upload {
        id: i16,
        strong: u16,
        weak: u16,
        length_ms: u16,
    },
    /// `count == 0` stops the effect.
    Play {
        id: i16,
        count: i32,
    },
    Erase {
        id: i16,
    },
}

/// Forwards [`FfRequest`]s while the app holds focus; stops everything otherwise.
pub struct Forwarder<S: FfSink> {
    sink: S,
    focused: bool,
    uploaded: BTreeSet<i16>,
    playing: BTreeSet<i16>,
}

impl<S: FfSink> Forwarder<S> {
    /// Starts unfocused: nothing plays until [`focus_gained`](Self::focus_gained).
    pub fn new(sink: S) -> Self {
        Self {
            sink,
            focused: false,
            uploaded: BTreeSet::new(),
            playing: BTreeSet::new(),
        }
    }

    pub fn focus_gained(&mut self) {
        self.focused = true;
    }

    pub fn handle(&mut self, req: FfRequest) -> io::Result<()> {
        match req {
            FfRequest::Upload {
                id,
                strong,
                weak,
                length_ms,
            } => {
                self.uploaded.insert(id);
                self.sink.upload(id, strong, weak, length_ms)
            }
            FfRequest::Play { id, count: 0 } => {
                self.playing.remove(&id);
                self.sink.stop(id)
            }
            FfRequest::Play { id, count } => {
                if !self.focused {
                    return Ok(());
                }
                self.playing.insert(id);
                self.sink.play(id, count)
            }
            FfRequest::Erase { id } => {
                self.uploaded.remove(&id);
                self.playing.remove(&id);
                self.sink.erase(id)
            }
        }
    }

    fn stop_all(&mut self) -> io::Result<()> {
        let mut first = Ok(());
        for id in std::mem::take(&mut self.playing) {
            if let Err(e) = self.sink.stop(id) {
                first = first.and(Err(e));
            }
        }
        first
    }

    /// The app lost input focus: silence the actuator and refuse new plays.
    pub fn focus_lost(&mut self) -> io::Result<()> {
        self.focused = false;
        self.stop_all()
    }

    /// The app closed its pad fd.
    pub fn close(&mut self) -> io::Result<()> {
        self.focused = false;
        self.stop_all()
    }

    /// Safe-return (system menu takeover).
    pub fn safe_return(&mut self) -> io::Result<()> {
        self.focused = false;
        self.stop_all()
    }
}

/// What a vibrator is, independent of its `/dev/input/eventN` number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VibratorIdentity {
    pub name: String,
    pub vendor: u16,
    pub product: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VibratorCandidate {
    pub path: String,
    pub name: String,
    pub vendor: u16,
    pub product: u16,
}

/// Pick the actuator by name + vendor/product, never by path or enumeration order.
pub fn select_vibrator<'a>(
    candidates: &'a [VibratorCandidate],
    id: &VibratorIdentity,
) -> Option<&'a VibratorCandidate> {
    candidates
        .iter()
        .find(|c| c.name == id.name && c.vendor == id.vendor && c.product == id.product)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uinput::{app_pad_uinput_spec, pad_uinput_spec, UinputSpec};
    use pf_input_ipc::PadDestination;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct Fake {
        log: Rc<RefCell<Vec<String>>>,
    }
    impl FfSink for Fake {
        fn upload(&mut self, id: i16, strong: u16, weak: u16, ms: u16) -> std::io::Result<()> {
            self.log
                .borrow_mut()
                .push(format!("up {id} {strong} {weak} {ms}"));
            Ok(())
        }
        fn play(&mut self, id: i16, count: i32) -> std::io::Result<()> {
            self.log.borrow_mut().push(format!("play {id} {count}"));
            Ok(())
        }
        fn stop(&mut self, id: i16) -> std::io::Result<()> {
            self.log.borrow_mut().push(format!("stop {id}"));
            Ok(())
        }
        fn erase(&mut self, id: i16) -> std::io::Result<()> {
            self.log.borrow_mut().push(format!("erase {id}"));
            Ok(())
        }
    }

    fn base() -> UinputSpec {
        UinputSpec {
            name: "x".into(),
            bus: 3,
            vendor: 1,
            product: 2,
            version: 1,
            keys: vec![0x130],
            rel: vec![],
            abs: vec![],
            ff_rumble: false,
        }
    }

    fn fwd() -> (Forwarder<Fake>, Rc<RefCell<Vec<String>>>) {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut f = Forwarder::new(Fake { log: log.clone() });
        f.focus_gained();
        (f, log)
    }

    #[test]
    fn app_pad_advertises_ff_rumble_only_with_actuator() {
        assert!(app_pad_uinput_spec(&base(), true).ff_rumble);
        assert!(!app_pad_uinput_spec(&base(), false).ff_rumble);
        for d in [
            PadDestination::Keyboard,
            PadDestination::Menu,
            PadDestination::Shell,
        ] {
            assert!(!pad_uinput_spec(&base(), d).unwrap().ff_rumble);
        }
        assert!(
            !pad_uinput_spec(&base(), PadDestination::App)
                .unwrap()
                .ff_rumble
        );
    }

    #[test]
    fn forwards_upload_play_erase_to_vibrator() {
        let (mut f, log) = fwd();
        f.handle(FfRequest::Upload {
            id: 3,
            strong: 100,
            weak: 50,
            length_ms: 200,
        })
        .unwrap();
        f.handle(FfRequest::Play { id: 3, count: 1 }).unwrap();
        f.handle(FfRequest::Play { id: 3, count: 0 }).unwrap();
        f.handle(FfRequest::Erase { id: 3 }).unwrap();
        assert_eq!(
            *log.borrow(),
            ["up 3 100 50 200", "play 3 1", "stop 3", "erase 3"]
        );
    }

    #[test]
    fn stops_on_focus_loss_close_and_safe_return() {
        for ev in ["focus", "close", "safe"] {
            let (mut f, log) = fwd();
            f.handle(FfRequest::Upload {
                id: 1,
                strong: 1,
                weak: 1,
                length_ms: 9,
            })
            .unwrap();
            f.handle(FfRequest::Play { id: 1, count: 1 }).unwrap();
            log.borrow_mut().clear();
            match ev {
                "focus" => f.focus_lost(),
                "close" => f.close(),
                _ => f.safe_return(),
            }
            .unwrap();
            assert_eq!(*log.borrow(), ["stop 1"], "{ev}");
            // Rumble must not restart without focus.
            f.handle(FfRequest::Play { id: 1, count: 1 }).unwrap();
            assert_eq!(log.borrow().len(), 1, "{ev}");
        }
    }

    #[test]
    fn vibrator_selected_by_identity() {
        let id = VibratorIdentity {
            name: "pf-vibrator".into(),
            vendor: 0x5046,
            product: 0x0200,
        };
        let c = |p: &str, n: &str, v, pr| VibratorCandidate {
            path: p.into(),
            name: n.into(),
            vendor: v,
            product: pr,
        };
        let a = [
            c("/dev/input/event0", "pf-vibrator", 1, 1),
            c("/dev/input/event1", "other", 0x5046, 0x0200),
            c("/dev/input/event7", "pf-vibrator", 0x5046, 0x0200),
        ];
        assert_eq!(select_vibrator(&a, &id).unwrap().path, "/dev/input/event7");
        let b = [a[2].clone(), a[0].clone()];
        assert_eq!(select_vibrator(&b, &id).unwrap().path, "/dev/input/event7");
        assert!(select_vibrator(&a[..2], &id).is_none());
    }

    #[test]
    fn udev_rule_matches_app_pad_spec() {
        let rule = include_str!("../systemd/70-pf-app-pad.rules");
        let s = pad_uinput_spec(&base(), PadDestination::App).unwrap();
        for needle in [
            format!("ATTRS{{name}}==\"{}\"", s.name),
            format!("ATTRS{{id/vendor}}==\"{:04x}\"", s.vendor),
            format!("ATTRS{{id/product}}==\"{:04x}\"", s.product),
            "SYMLINK+=\"input/pf-app-pad\"".to_string(),
        ] {
            assert!(rule.contains(&needle), "missing {needle}");
        }
    }
}
