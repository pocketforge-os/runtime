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
            self.log.borrow_mut().push(format!("up {id} {strong} {weak} {ms}"));
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
        for d in [PadDestination::Keyboard, PadDestination::Menu, PadDestination::Shell] {
            assert!(!pad_uinput_spec(&base(), d).unwrap().ff_rumble);
        }
        assert!(!pad_uinput_spec(&base(), PadDestination::App).unwrap().ff_rumble);
    }

    #[test]
    fn forwards_upload_play_erase_to_vibrator() {
        let (mut f, log) = fwd();
        f.handle(FfRequest::Upload { id: 3, strong: 100, weak: 50, length_ms: 200 }).unwrap();
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
            f.handle(FfRequest::Upload { id: 1, strong: 1, weak: 1, length_ms: 9 }).unwrap();
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
        let id = VibratorIdentity { name: "pf-vibrator".into(), vendor: 0x5046, product: 0x0200 };
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
