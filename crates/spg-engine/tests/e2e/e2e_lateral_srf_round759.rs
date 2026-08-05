//! Round 759 (F31-B8b) — a set-returning FROM item's argument can
//! reference an earlier FROM item (PG's implicit LATERAL), including
//! through an ARRAY constructor, and unnest(tsvector) works in a
//! JOINED FROM list. Two root causes, PG18-measured:
//!
//! - the parser's correlated-SRF detector (`expr_has_any_column`) had
//!   no Array / ArraySubscript arms, so `unnest(ARRAY[x, x + 1])`
//!   never wrapped into the lateral channel and the eager peer eval
//!   answered `column "x" does not exist` (inside a scalar subquery
//!   the same miss surfaced as "subquery reached row eval — engine
//!   resolver bug", the round-753 audit's sentence);
//! - the joined-path materialiser lacked round-758's
//!   unnest(tsvector) arm, so `FROM unnest(tsv) t, unnest(t.positions)`
//!   died before the lateral half even ran.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join(";"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn round759_srf_args_reference_earlier_from_items() {
    let mut e = Engine::new();
    // Column nested in an ARRAY constructor — the detector's blind spot.
    assert_eq!(
        one(&mut e, "SELECT y FROM unnest(ARRAY[1,2]) x, unnest(ARRAY[x, x+1]) y ORDER BY y"),
        "1;2;2;3"
    );
    assert_eq!(
        one(&mut e, "SELECT y FROM unnest(ARRAY[1,2]) x, LATERAL unnest(ARRAY[x]) y ORDER BY y"),
        "1;2"
    );
    // The round-753 audit probe, end to end: max position of a
    // tsvector through two chained unnests, inside a scalar subquery.
    assert_eq!(
        one(
            &mut e,
            "SELECT (SELECT max(p) FROM unnest(to_tsvector('simple','a b a')) t, \
             unnest(t.positions) p)"
        ),
        "3"
    );
    assert_eq!(
        one(&mut e, "SELECT (SELECT sum(y) FROM unnest(ARRAY[1,2]) x, unnest(ARRAY[x, x+1]) y)"),
        "8"
    );
}

#[test]
fn round759_table_sourced_lateral_still_answers_as_pg() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE lt (id INT, arr INT[])").unwrap();
    e.execute("INSERT INTO lt VALUES (1, ARRAY[10,20]), (2, ARRAY[30])").unwrap();
    for sql in [
        "SELECT id, e FROM lt, unnest(lt.arr) e ORDER BY id, e",
        "SELECT id, e FROM lt, unnest(arr) e ORDER BY id, e",
        "SELECT id, e FROM lt JOIN LATERAL unnest(lt.arr) e ON true ORDER BY id, e",
    ] {
        assert_eq!(one(&mut e, sql), "1|10;1|20;2|30", "{sql}");
    }
    assert_eq!(
        one(&mut e, "SELECT id, g FROM lt, generate_series(1, lt.id) g ORDER BY id, g"),
        "1|1;2|1;2|2"
    );
}
