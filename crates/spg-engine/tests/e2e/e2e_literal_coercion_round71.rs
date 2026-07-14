//! v7.39 (read01 round 71) — the unknown-literal sweep.
//!
//! Round 70 found ONE hole of this shape (`tags @> '{b}'` read as JSON). Holes
//! of a shape come in families, so this round swept the same question across
//! every type that has an operator taking a bare literal: ranges, inet,
//! tsvector, uuid, arrays. Three more fell out — and one of them was not a
//! coercion bug at all.
//!
//!   - `r && '[4,11)'` — a RANGE beside a literal went to the INET operator.
//!   - `ts @@ 'a'` — PG reads the literal through the TSQUERY *input* function
//!     (no stemming — that is what makes it different from `to_tsquery`).
//!   - `array_remove(tags, 'a')` — errored with **"first arg must be array, got
//!     TextArray"**. A message that contradicts itself: a TextArray IS an array.
//!     The function simply had no TEXT-array arm, and its guard reported the arm
//!     it LACKED as if the caller were wrong. A self-contradicting error message
//!     is worth reading twice — it is usually pointing at the wrong thing.
//!
//! Byte-locked against live PG18.4, with in-place MVCC on.

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

fn seeded() -> Engine {
    let mut e = Engine::new();
    ok(
        &mut e,
        "CREATE TABLE t (id int, r int4range, ip inet, ts tsvector, tags text[])",
    );
    ok(
        &mut e,
        "INSERT INTO t VALUES \
         (1,'[1,5)','10.0.0.1','a b'::tsvector,'{a,b}'), \
         (2,'[10,20)','192.168.1.5','c d'::tsvector,'{c}')",
    );
    e
}

#[test]
fn a_range_beside_a_literal_is_a_range() {
    let mut e = seeded();
    // Used to reach the INET operator: "requires INET/CIDR/TEXT operands".
    assert_eq!(
        r1(&mut e, "SELECT string_agg(id::text, ',' ORDER BY id) FROM t WHERE r && '[4,11)'"),
        "1,2"
    );
    // The containment reading of the same operator is untouched.
    assert_eq!(
        r1(&mut e, "SELECT string_agg(id::text, ',' ORDER BY id) FROM t WHERE r @> 3"),
        "1"
    );
}

#[test]
fn a_tsvector_matches_a_bare_literal_query() {
    let mut e = seeded();
    assert_eq!(
        r1(&mut e, "SELECT string_agg(id::text, ',' ORDER BY id) FROM t WHERE ts @@ 'a'"),
        "1"
    );
    // The literal goes through the tsquery INPUT function, not to_tsquery: no
    // stemming. `'b & c'` is a real tsquery, and matches nothing here.
    assert_eq!(
        r1(&mut e, "SELECT count(*) FROM t WHERE ts @@ 'b & c'"),
        "0"
    );
    assert_eq!(
        r1(&mut e, "SELECT string_agg(id::text, ',') FROM t WHERE ts @@ 'c & d'"),
        "2"
    );
}

#[test]
fn array_remove_handles_text_arrays() {
    let mut e = seeded();
    assert_eq!(
        r1(&mut e, "SELECT array_to_string(array_remove(tags,'a'),'|') FROM t WHERE id=1"),
        "b"
    );
    // The int arrays it always handled still work.
    assert_eq!(
        r1(&mut e, "SELECT array_to_string(array_remove(ARRAY[1,2,3], 2), '|')"),
        "1|3"
    );
}

#[test]
fn the_inet_reading_of_the_same_operators_survives() {
    let mut e = seeded();
    assert_eq!(
        r1(&mut e, "SELECT string_agg(id::text, ',') FROM t WHERE ip << '192.168.0.0/16'"),
        "2"
    );
}
