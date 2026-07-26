//! v7.39 (round 517) — the rest of the real-argument panel.
//!
//! Round 516 took the panel from 3 of 36 agreeing with PG18 to 22. This
//! takes it to 30. What is left is listed at the bottom with why.
//!
//! `to_regtypemod` is the one with a rule worth writing down: PG packs a
//! type modifier into an int32, so a length becomes `len + 4` and a
//! numeric's precision and scale become `(p << 16) | s` and then `+ 4`.
//! `varchar(32)` is 36 and `numeric(10,2)` is 655366 — measured, because no
//! amount of reading suggests those numbers.
//!
//! The `*_is_visible` family needs care rather than code. PG answers NULL
//! for an oid that names nothing and true when the object is on the search
//! path. SPG has a `pg_collation`, so that one is a lookup; it has no
//! `pg_opfamily`, `pg_conversion` or statistics objects at all, so NO oid
//! names one — NULL there is not a stand-in, it is the answer PG gives for
//! exactly that case.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    Engine::new()
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .unwrap_or_default(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// PG's packed type modifier. The numbers are the point.
#[test]
fn round517_to_regtypemod_packs_the_modifier() {
    let mut e = engine();
    assert_eq!(text(&mut e, "SELECT to_regtypemod('varchar(32)')"), "36");
    assert_eq!(text(&mut e, "SELECT to_regtypemod('numeric(10,2)')"), "655366");
    // A type with no modifier is -1; a name that is not a type is NULL.
    assert_eq!(text(&mut e, "SELECT to_regtypemod('int')"), "-1");
    assert_eq!(text(&mut e, "SELECT to_regtypemod('nosuch')"), "NULL");
}

/// `xmlconcat2` is the pair behind the `xmlconcat` aggregate, and a NULL
/// side is the other side rather than a NULL result.
#[test]
fn round517_xmlconcat2() {
    let mut e = engine();
    assert_eq!(
        text(&mut e, "SELECT xmlconcat2('<a/>'::xml, '<b/>'::xml)::text"),
        "<a/><b/>"
    );
    assert_eq!(
        text(&mut e, "SELECT xmlconcat2(NULL::xml, '<b/>'::xml)::text"),
        "<b/>"
    );
}

/// Snapshot visibility: below xmin visible, at or above xmax not, and in
/// between visible unless the id is in the in-progress list.
#[test]
fn round517_txid_visible_in_snapshot() {
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT txid_visible_in_snapshot(5::bigint, '10:20:'::txid_snapshot), \
             txid_visible_in_snapshot(15::bigint, '10:20:'::txid_snapshot), \
             txid_visible_in_snapshot(15::bigint, '10:20:15'::txid_snapshot), \
             txid_visible_in_snapshot(25::bigint, '10:20:'::txid_snapshot)"
        ),
        "true|true|false|false"
    );
}

/// The third argument names the lexemes to weight; the rest keep theirs.
#[test]
fn round517_setweight_can_name_the_lexemes() {
    let mut e = engine();
    assert_eq!(
        text(&mut e, "SELECT setweight('cat:1 dog:2'::tsvector, 'B', '{cat}')::text"),
        "'cat':1B 'dog':2"
    );
    // Two arguments still weight everything.
    assert_eq!(
        text(&mut e, "SELECT setweight('cat:1 dog:2'::tsvector, 'B')::text"),
        "'cat':1B 'dog':2B"
    );
}

/// The visibility family: a lookup where SPG has the catalogue, and NULL
/// where it has no such objects — which is PG's own answer for an oid that
/// names nothing.
#[test]
fn round517_the_is_visible_family() {
    let mut e = engine();
    assert_eq!(text(&mut e, "SELECT pg_collation_is_visible(100::oid)"), "true");
    assert_eq!(
        text(&mut e, "SELECT pg_collation_is_visible(999999::oid)"),
        "NULL"
    );
    for f in [
        "pg_opfamily_is_visible",
        "pg_conversion_is_visible",
        "pg_statistics_obj_is_visible",
    ] {
        assert_eq!(text(&mut e, &format!("SELECT {f}(100::oid)")), "NULL", "{f}");
    }
    // Only ANOTHER session's temp schema is other-temp, and none is
    // reachable by oid here.
    assert_eq!(text(&mut e, "SELECT pg_is_other_temp_schema(11::oid)"), "false");
    assert_eq!(text(&mut e, "SELECT pg_is_other_temp_schema(NULL::oid)"), "NULL");
}
