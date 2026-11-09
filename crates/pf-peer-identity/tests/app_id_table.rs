//! Consumes the shared leaf -> app_id table (`fixtures/app_id_from_unit.tsv`) for `app_id_from_unit`.

#[test]
fn app_id_from_unit_matches_shared_table() {
    let table = include_str!("fixtures/app_id_from_unit.tsv");
    let (mut pos, mut neg) = (0, 0);
    for line in table.lines().filter(|l| !l.starts_with('#')) {
        let (leaf, want) = line.split_once('\t').expect("leaf<TAB>app_id");
        let got = pf_peer_identity::app_id_from_unit(leaf);
        let want = (want != "-").then(|| want.to_owned());
        assert_eq!(got, want, "leaf {leaf:?}");
        if want.is_some() {
            pos += 1
        } else {
            neg += 1
        }
    }
    assert!(pos >= 3 && neg >= 5, "table too small: {pos}/{neg}");
}
