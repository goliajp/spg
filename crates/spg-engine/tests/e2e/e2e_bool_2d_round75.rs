//! v7.39 (read01 round 75) — `bool[][]`: the one typed 2-D array SPG needs.
//!
//! Round 73 left this as a residual and called it "only the type NAME is wrong".
//! **That was wrong, and measuring it is what showed it.** The value is wrong
//! too:
//!
//!     SELECT (ARRAY[ARRAY[true,false]])::text;
//!     PG:   {{t,f}}
//!     SPG:  {{true,false}}
//!
//! And the two requirements CANNOT both be met by a text-backed 2-D array:
//! rendering the whole array wants `t`, while subscripting a cell to text wants
//! `false`. Whichever string you store, one of them is wrong. A typed variant is
//! not decoration here — it is the only correct representation.
//!
//! BOOL is the ONLY element type with this problem: it is the one whose ARRAY
//! rendering differs from its scalar rendering. Dates, uuids, numerics and text
//! render the same either way — verified against PG before adding anything — so
//! `bool[][]` is the only typed 2-D variant SPG has to carry.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn r1(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn a_bool_matrix_reports_and_renders_like_pg() {
    let mut e = Engine::new();
    assert_eq!(r1(&mut e, "SELECT pg_typeof(ARRAY[ARRAY[true,false]])"), "boolean[]");
    // `t` / `f` INSIDE the array…
    assert_eq!(r1(&mut e, "SELECT (ARRAY[ARRAY[true,false]])::text"), "{{t,f}}");
    // …and `false` when a cell is pulled out as a scalar. The pair of these is
    // what a text-backed 2-D could never satisfy at once.
    assert_eq!(r1(&mut e, "SELECT (ARRAY[ARRAY[true,false]])[1][2]::text"), "false");
}

#[test]
fn the_other_element_types_are_untouched() {
    // They render the same in an array and out of it, so they stay on the
    // text-backed 2-D — no variant, no storage change.
    let mut e = Engine::new();
    assert_eq!(r1(&mut e, "SELECT (ARRAY[ARRAY[1,2],ARRAY[3,4]])::text"), "{{1,2},{3,4}}");
    assert_eq!(r1(&mut e, "SELECT (ARRAY[ARRAY['a','b']])::text"), "{{a,b}}");
    assert_eq!(
        r1(&mut e, "SELECT (ARRAY[ARRAY['2024-01-01'::date]])::text"),
        "{{2024-01-01}}"
    );
    assert_eq!(r1(&mut e, "SELECT array_dims(ARRAY[ARRAY[true,false]])"), "[1:1][1:2]");
    assert_eq!(r1(&mut e, "SELECT array_length(ARRAY[ARRAY[1,2],ARRAY[3,4]],2)::text"), "2");
}

#[test]
fn a_bool_matrix_survives_a_snapshot() {
    // The new codec tag, round-tripped through a real snapshot.
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE m (id int, g bool[][])");
    ok(&mut e, "INSERT INTO m VALUES (1, ARRAY[ARRAY[true,false],ARRAY[false,true]])");
    let bytes = e.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let t = cat.get("m").unwrap();
    let cell = &t.rows()[0].values[1];
    assert!(
        matches!(cell, spg_storage::Value::BoolArray2D(rows) if rows.len() == 2 && rows[0][1] == Some(false)),
        "{cell:?}"
    );
}
