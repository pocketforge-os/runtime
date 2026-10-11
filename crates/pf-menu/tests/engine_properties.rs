use pf_menu::{
    Bind, Cond, Confirm, EngineOptions, Feedback, Item, ItemId, ItemKind, Menu, MenuEffect,
    MenuEngine, MenuInput, MenuValue, Msg, Page, PageId, PrefsSchemas, Section, FORMAT_V1,
};
use pf_ports::MonotonicTime;

fn action(id: &str) -> Item {
    Item::new(
        id,
        Msg::text(id),
        ItemKind::Action {
            confirm: None,
            stay_open: true,
        },
    )
}

fn link(id: &str, page: &str) -> Item {
    Item::new(
        id,
        Msg::text(id),
        ItemKind::Link {
            page: PageId(page.into()),
        },
    )
}

fn page(id: &str, items: Vec<Item>) -> Page {
    Page {
        id: PageId(id.into()),
        title: Msg::text(id),
        tabs: vec![],
        primary: None,
        sections: vec![Section {
            header: None,
            footer: None,
            items,
        }],
    }
}

fn test_menu() -> Menu {
    let mut confirm = action("confirm");
    confirm.kind = ItemKind::Action {
        confirm: Some(Confirm {
            title: Msg::text("Confirm?"),
            body: None,
            confirm_label: Msg::text("Confirm"),
            destructive: true,
        }),
        stay_open: true,
    };
    let choice = Item::new(
        "choice",
        Msg::text("Choice"),
        ItemKind::Choice {
            bind: Bind::Host("choice".into()),
            options: ["one", "two", "three"]
                .into_iter()
                .map(|value| pf_menu::ChoiceOption {
                    value: value.into(),
                    label: Msg::text(value),
                    detail: None,
                })
                .collect(),
            present: pf_menu::Present::Picker,
        },
    );
    Menu {
        format: FORMAT_V1.into(),
        id: "properties".into(),
        root: PageId("root".into()),
        pages: vec![
            page(
                "root",
                vec![action("root-a"), link("to-child", "child"), confirm, choice],
            ),
            page(
                "child",
                vec![action("child-a"), action("child-b"), action("child-c")],
            ),
        ],
    }
}

fn now(step: u64) -> MonotonicTime {
    MonotonicTime::from_nanos(step * 10_000_000)
}

fn id(engine: &MenuEngine) -> Option<&str> {
    engine.focus().map(|index| engine.item(index).id.0.as_str())
}

#[derive(Clone, Copy)]
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

fn assert_focus_invariant(engine: &MenuEngine, seed: u64, step: usize) {
    let visible: Vec<_> = engine.visible_items(engine.page()).collect();
    match engine.focus() {
        Some(focus) => assert!(
            visible.contains(&focus) && engine.is_visible(focus),
            "seed={seed:#x} step={step}: focus {:?} is not visible on page {:?}",
            focus,
            engine.page()
        ),
        None => assert!(
            visible.is_empty(),
            "seed={seed:#x} step={step}: missing focus with visible rows"
        ),
    }
}

#[test]
fn focus_stays_visible_under_random_inputs_and_page_edits() {
    const SEEDS: &[u64] = &[0, 1, 0x5eed, 0xdead_beef, u64::MAX];
    for &seed in SEEDS {
        let mut rng = SplitMix64(seed);
        let mut engine =
            MenuEngine::new(test_menu(), &PrefsSchemas, EngineOptions::default()).unwrap();
        for step in 0..500 {
            match rng.next() % 10 {
                0..=5 => {
                    let input = match rng.next() % 10 {
                        0 => MenuInput::Up,
                        1 => MenuInput::Down,
                        2 => MenuInput::Left,
                        3 => MenuInput::Right,
                        4 => MenuInput::Activate,
                        5 => MenuInput::Back,
                        6 => MenuInput::PageUp,
                        7 => MenuInput::PageDown,
                        8 => MenuInput::ScanStep,
                        _ => MenuInput::ScanSelect,
                    };
                    let _ = engine.input(input, now(step as u64));
                }
                _ => {
                    let selected = (rng.next() % 3) as usize;
                    let hide = rng.next() & 1 == 0;
                    engine
                        .edit(&PrefsSchemas, |menu| {
                            let items = &mut menu.pages[1].sections[0].items;
                            items[selected].visible = if hide { Cond::Never } else { Cond::Always };
                            if rng.next() & 1 == 0 {
                                items.rotate_left(1);
                            } else {
                                items.rotate_right(1);
                            }
                        })
                        .unwrap();
                }
            }
            assert_focus_invariant(&engine, seed, step);
        }
    }
}

#[test]
fn back_always_unwinds_to_root_then_closes() {
    for seed in 0..128_u64 {
        let mut rng = SplitMix64(seed);
        let mut engine =
            MenuEngine::new(test_menu(), &PrefsSchemas, EngineOptions::default()).unwrap();
        engine
            .set_value(
                engine.item_index(&ItemId("choice".into())).unwrap(),
                MenuValue::Choice(0),
            )
            .unwrap();
        for step in 0..100 {
            let input = match rng.next() % 6 {
                0 => MenuInput::Up,
                1 => MenuInput::Down,
                2 => MenuInput::Activate,
                3 => MenuInput::Left,
                4 => MenuInput::Right,
                _ => MenuInput::Back,
            };
            let _ = engine.input(input, now(step));
        }

        let mut closed = false;
        for step in 100..110 {
            if engine
                .input(MenuInput::Back, now(step))
                .contains(&MenuEffect::Close)
            {
                closed = true;
                break;
            }
        }
        assert!(closed, "seed={seed:#x}: Back did not close");
        assert_eq!(engine.depth(), 1, "seed={seed:#x}: not at root");
        assert_eq!(engine.page_ref(engine.page()).id.0, "root");
    }
}

#[test]
fn wrapping_and_page_memory_survive_reordering_edits() {
    let mut engine = MenuEngine::new(test_menu(), &PrefsSchemas, EngineOptions::default()).unwrap();
    assert_eq!(id(&engine), Some("root-a"));
    let effects = engine.input(MenuInput::Up, now(0));
    assert!(effects.contains(&MenuEffect::Feedback(Feedback::Boundary)));
    assert_eq!(id(&engine), Some("choice"));

    engine.input(MenuInput::Up, now(1));
    engine.input(MenuInput::Up, now(2));
    engine.input(MenuInput::Activate, now(3));
    assert_eq!(engine.page_ref(engine.page()).id.0, "child");
    engine.input(MenuInput::Down, now(4));
    assert_eq!(id(&engine), Some("child-b"));
    engine.input(MenuInput::Back, now(5));

    let mut rng = SplitMix64(0xc0ffee);
    for round in 0..100 {
        engine
            .edit(&PrefsSchemas, |menu| {
                let items = &mut menu.pages[1].sections[0].items;
                if rng.next() & 1 == 0 {
                    items.rotate_left(1);
                } else {
                    items.rotate_right(1);
                }
                for item in items.iter_mut() {
                    item.visible = if item.id.0 == "child-b" || rng.next() & 3 != 0 {
                        Cond::Always
                    } else {
                        Cond::Never
                    };
                }
            })
            .unwrap();
        assert_focus_invariant(&engine, 0xc0ffee, round);
    }

    while id(&engine) != Some("to-child") {
        engine.input(MenuInput::Up, now(200));
    }
    engine.input(MenuInput::Activate, now(201));
    assert_eq!(id(&engine), Some("child-b"));
}
