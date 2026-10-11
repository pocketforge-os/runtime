use pf_menu::kind::{check_numbering, FROZEN_V1, SDK_V1};
use pf_menu::{
    Bind, BindSchemas, ChoiceOption, Item, ItemKind, Menu, MenuError, Msg, PrefsSchemas,
    SchemaLookup, ValueSchema,
};

const FIXTURES: &[(&str, &str)] = &[
    (
        "system-menu.json",
        include_str!("fixtures/system-menu.json"),
    ),
    (
        "settings-root.json",
        include_str!("fixtures/settings-root.json"),
    ),
    (
        "settings-display.json",
        include_str!("fixtures/settings-display.json"),
    ),
    (
        "confirm-action.json",
        include_str!("fixtures/confirm-action.json"),
    ),
];

#[test]
fn kind_numbering_is_frozen_for_the_sdk_contract() {
    check_numbering(FROZEN_V1).unwrap();
    check_numbering(SDK_V1).unwrap();
    assert!(check_numbering(&[("PF_MENU_RANGE", 4)]).is_err());
    assert_eq!(SDK_V1[3], ("PF_MENU_RANGE", 3));
}

#[test]
fn golden_fixtures_round_trip_and_match_platform() {
    let platform = std::env::var_os("PF_PLATFORM_DIR").map(std::path::PathBuf::from);
    for (name, text) in FIXTURES {
        let menu = Menu::from_json(text).unwrap_or_else(|error| panic!("{name}: {error}"));
        menu.validate(&PrefsSchemas)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(
            Menu::from_json(&menu.to_json_pretty()).unwrap(),
            menu,
            "{name} does not round-trip"
        );
        if let Some(root) = &platform {
            let peer = root.join("schemas/menu-page/fixtures").join(name);
            assert_eq!(
                std::fs::read_to_string(&peer).unwrap(),
                *text,
                "{}",
                peer.display()
            );
        }
    }
}

struct OneSchema(ValueSchema);

impl BindSchemas for OneSchema {
    fn lookup(&self, bind: &Bind) -> SchemaLookup {
        match bind {
            Bind::Host(key) if key == "tested" => SchemaLookup::Known(self.0.clone()),
            _ => SchemaLookup::Unknown,
        }
    }
}

fn choice(values: &[&str]) -> Menu {
    let item = Item::new(
        "choice",
        Msg::text("Choice"),
        ItemKind::Choice {
            bind: Bind::Host("tested".into()),
            options: values
                .iter()
                .map(|value| ChoiceOption {
                    value: (*value).into(),
                    label: Msg::text(value),
                    detail: None,
                })
                .collect(),
            present: pf_menu::Present::Inline,
        },
    );
    serde_json::from_value(serde_json::json!({
        "format": "pocketforge.menu/1",
        "id": "bound",
        "root": "root",
        "pages": [{"id":"root", "title":{"text":"Root"}, "sections":[{"items":[item]}]}]
    }))
    .unwrap()
}

#[test]
fn bound_schema_accepts_only_values_the_backend_accepts() {
    let schemas = OneSchema(ValueSchema::Enum {
        variants: vec!["safe".into(), "fast".into()],
    });
    choice(&["safe", "fast"]).validate(&schemas).unwrap();
    assert!(matches!(
        choice(&["safe", "invalid"]).validate(&schemas),
        Err(MenuError::BindMismatch { .. })
    ));
}
