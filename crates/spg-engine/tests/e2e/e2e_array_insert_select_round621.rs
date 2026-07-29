//! v7.39 (round 621) — array data could not be moved between tables.
//!
//! `INSERT INTO b SELECT * FROM a` failed outright whenever `a` had an array
//! column, for EVERY array type, and so did `CREATE TABLE … AS SELECT` over
//! one. The error told the caller to "add an explicit CAST in the inner
//! SELECT"; the cast did not help, because the value still arrived at the same
//! place on its way back to a literal. So the advice was wrong as well as the
//! behaviour, and there was no spelling of the statement that worked.
//!
//! A subquery result is materialised by turning the Value back into an AST
//! literal. Arrays had no arm there. Two cuts of the obvious fix — the TEXT
//! form plus a cast, which is the road UUID, BYTEA and TID take — both failed,
//! and the second one is the interesting one: neither the internal spelling
//! (`::int4[]`) nor the SQL spelling (`::integer[]`) resolves when the target
//! is CONSTRUCTED as `CastTarget::Named`, though `SELECT '{1,2}'::integer[]`
//! typed by hand is fine. The parser does not produce a `Named` for those.
//!
//! Rebuilding the literal sidesteps the question entirely: `ARRAY[1,2]` is
//! what the row was written with in the first place. Each element goes back
//! through the same materialiser, so a NULL element stays NULL and an element
//! type that cannot be materialised still says so rather than being quietly
//! dropped.
//!
//! Measured and NOT closed, each a gap of its own and upstream of this one:
//! a `BYTEA[]` renders to text correctly but reading that text back answers
//! TEXT rather than BYTEA[], and a `JSON[]` cannot be inserted at all
//! (`ARRAY['{}'::JSON]` is refused as TEXT[]). Both are left out of the
//! rebuild, so they keep the old error rather than round-tripping through the
//! wrong type.
//!
//! Every shape below was checked against live PG18 and matched byte for byte.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE b0 (i INT, ia INT[], ta TEXT[], na NUMERIC[], ba BOOL[], \
         da DATE[], bga BIGINT[], sa SMALLINT[], fa FLOAT8[], ua UUID[])",
    )
    .unwrap();
    e.execute(
        "INSERT INTO b0 VALUES (1, ARRAY[1,2], ARRAY['x','y'], ARRAY[1.5], ARRAY[true], \
         ARRAY[DATE '2020-01-02'], ARRAY[9223372036854775807], ARRAY[32767::SMALLINT], \
         ARRAY[1.5::FLOAT8], ARRAY['00000000-0000-0000-0000-000000000001'::UUID])",
    )
    .unwrap();
    // A row of NULL arrays, an empty one, and one with a NULL element and text
    // that has to stay quoted.
    e.execute("INSERT INTO b0 VALUES (2, NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL)")
        .unwrap();
    e.execute(
        "INSERT INTO b0 VALUES (3, ARRAY[]::INT[], ARRAY[]::TEXT[], NULL,NULL,NULL,NULL,NULL,NULL,NULL)",
    )
    .unwrap();
    e.execute(
        "INSERT INTO b0 VALUES (4, ARRAY[1,NULL,2], ARRAY['a b','c,d',NULL], \
         NULL,NULL,NULL,NULL,NULL,NULL,NULL)",
    )
    .unwrap();
    e
}

/// The statement that did not work at all.
#[test]
fn round621_insert_select_carries_arrays() {
    let mut e = seed();
    e.execute("CREATE TABLE b1 (LIKE b0)").unwrap();
    e.execute("INSERT INTO b1 SELECT * FROM b0").unwrap();
    assert_eq!(
        vals(&mut e, "SELECT i, ia, ta, na, ba FROM b1 ORDER BY i"),
        vec![
            "1|{1,2}|{x,y}|{1.5}|{t}",
            "2|NULL|NULL|NULL|NULL",
            "3|{}|{}|NULL|NULL",
            r#"4|{1,NULL,2}|{"a b","c,d",NULL}|NULL|NULL"#,
        ],
        "a NULL array, an EMPTY array, a NULL ELEMENT, and text that has to \
         stay quoted — all four are separate ways for a rebuild to go wrong"
    );
    assert_eq!(
        vals(&mut e, "SELECT i, da, bga, sa, fa, ua FROM b1 WHERE i = 1"),
        vec![
            "1|{2020-01-02}|{9223372036854775807}|{32767}|{1.5}|\
             {00000000-0000-0000-0000-000000000001}"
        ],
        "and the element types that materialise through a cast of their own"
    );
}

/// The other ways an array reaches a table.
#[test]
fn round621_the_other_spellings() {
    let mut e = seed();
    e.execute("CREATE TABLE b2 (i INT, ia INT[])").unwrap();
    e.execute("INSERT INTO b2 (i, ia) SELECT i, ia FROM b0").unwrap();
    assert_eq!(
        vals(&mut e, "SELECT i, ia FROM b2 ORDER BY i"),
        vec!["1|{1,2}", "2|NULL", "3|{}", "4|{1,NULL,2}"],
        "a named column list"
    );
    e.execute("CREATE TABLE b3 AS SELECT i, ta FROM b0").unwrap();
    assert_eq!(
        vals(&mut e, "SELECT i, ta FROM b3 ORDER BY i"),
        vec!["1|{x,y}", "2|NULL", "3|{}", r#"4|{"a b","c,d",NULL}"#],
        "CREATE TABLE … AS SELECT, which could not carry one either"
    );
    e.execute("CREATE TABLE b4 (i INT, ia INT[])").unwrap();
    e.execute("INSERT INTO b4 (i, ia) SELECT i, ARRAY[i, i*2] FROM b0")
        .unwrap();
    assert_eq!(
        vals(&mut e, "SELECT i, ia FROM b4 ORDER BY i"),
        vec!["1|{1,2}", "2|{2,4}", "3|{3,6}", "4|{4,8}"],
        "an array COMPUTED in the inner select"
    );
    e.execute("CREATE TABLE b5 (i INT, ia INT[])").unwrap();
    e.execute("INSERT INTO b5 (i, ia) SELECT i, ia FROM b0 WHERE ia IS NOT NULL")
        .unwrap();
    assert_eq!(
        vals(&mut e, "SELECT i FROM b5 ORDER BY i"),
        vec!["1", "3", "4"]
    );
    e.execute("CREATE TABLE b6 (i INT, ia INT[])").unwrap();
    e.execute("INSERT INTO b6 (i, ia) SELECT i, ia::INT[] FROM b0")
        .unwrap();
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM b6"),
        vec!["4"],
        "the explicit CAST the old error told the caller to add, which did \
         not help either"
    );
}

/// What lands is an array, not text that looks like one.
#[test]
fn round621_what_lands_is_an_array() {
    let mut e = seed();
    e.execute("CREATE TABLE b7 (i INT, ia INT[])").unwrap();
    e.execute("INSERT INTO b7 (i, ia) SELECT i, ia FROM b0").unwrap();
    assert_eq!(
        vals(&mut e, "SELECT i, array_length(ia,1), ia[1] FROM b7 ORDER BY i"),
        vec!["1|2|1", "2|NULL|NULL", "3|NULL|NULL", "4|3|1"],
        "subscript and length work on it, so it is an array"
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM b7 WHERE ia @> ARRAY[1]"),
        vec!["2"],
        "and the containment operator finds it"
    );
    assert_eq!(
        vals(&mut e, "SELECT pg_typeof(ia) FROM b7 WHERE i = 1"),
        vec!["integer[]"]
    );
}
