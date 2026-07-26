//! read01 round 485 — the ordering comparator's fast dispatch, and the
//! projection buffer a duplicate row no longer allocates.
//!
//! Two changes, both on the `SELECT DISTINCT … ORDER BY` path:
//!
//! * `orderby::value_cmp` answers a same-variant scalar before the
//!   NumericBig gate and the NumericKind rank block. The claim is that
//!   neither gate can fire on such a pair, so these pin the gates on the
//!   pairs that DO need them — a bignum beyond i128, numeric scale
//!   folding, float specials, bpchar blank-insensitivity — plus ordering
//!   direction, because a comparator with `cmp` reversed still passes
//!   every equality test.
//!
//! * The single-table scan keeps one projection buffer for the whole scan
//!   instead of allocating a `Vec<Value>` per input row; a row that
//!   duplicates an earlier one leaves the buffer in place. A buffer that
//!   is not cleared properly shows up as values leaking from one row into
//!   the next, so these pin duplicates adjacent to non-duplicates, NULLs,
//!   and the every-row-survives shape that takes the buffer each time.
//!
//! Every expectation below is PG18's answer, read off `psql -tA` rather
//! than reasoned about. The one transcription is NULL: psql prints it as
//! an empty field and this harness prints `NULL`, so the pins spell it the
//! harness's way — same rows, same order, same NULL placement.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> String {
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
        other => panic!("{sql} -> {other:?}"),
    }
}

// ---- the gates the fast dispatch skips must still fire ----

#[test]
fn round485_bignum_gate_still_orders_beyond_i128() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE n (v NUMERIC)").unwrap();
    e.execute(
        "INSERT INTO n VALUES (7), (1234567890123456789012345678901234567890), \
         (-1234567890123456789012345678901234567890), (0)",
    )
    .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT v::text AS t FROM n ORDER BY v"),
        "-1234567890123456789012345678901234567890;0;7;\
         1234567890123456789012345678901234567890"
    );
}

#[test]
fn round485_numeric_scale_still_folds_under_distinct() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE n (v NUMERIC)").unwrap();
    e.execute("INSERT INTO n VALUES (1), (1.0), (1.00)").unwrap();
    // PG18: one row. Scale-aware equality, not textual.
    assert_eq!(rows(&mut e, "SELECT count(*) FROM (SELECT DISTINCT v FROM n) s"), "1");
}

#[test]
fn round485_float_specials_still_rank() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE f (v FLOAT8)").unwrap();
    e.execute("INSERT INTO f VALUES ('NaN'), (1.5), ('-Infinity'), ('Infinity'), (-2.5), (0)")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT v::text AS t FROM f ORDER BY v"),
        "-Infinity;-2.5;0;1.5;Infinity;NaN"
    );
}

#[test]
fn round485_bpchar_still_ignores_trailing_blanks() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE c (v CHAR(6))").unwrap();
    e.execute("INSERT INTO c VALUES ('ab'), ('ab    '), ('abc'), ('b')")
        .unwrap();
    // PG18: 'ab' and 'ab    ' are one value, so three distinct rows.
    assert_eq!(rows(&mut e, "SELECT count(*) FROM (SELECT DISTINCT v FROM c) s"), "3");
    assert_eq!(
        rows(&mut e, "SELECT v::text AS t FROM c ORDER BY v, t"),
        "ab;ab;abc;b"
    );
}

#[test]
fn round485_cross_width_integers_still_widen() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE i (a SMALLINT, b INT, c BIGINT)").unwrap();
    e.execute("INSERT INTO i VALUES (9, 1000, -5)").unwrap();
    // Values from three widths ordered together: by magnitude, not by
    // spelling ("1000" would sort before "9" lexicographically).
    assert_eq!(
        rows(
            &mut e,
            "SELECT v::text AS t FROM (SELECT a AS v FROM i UNION ALL SELECT b FROM i \
             UNION ALL SELECT c FROM i) u ORDER BY v"
        ),
        "-5;9;1000"
    );
}

#[test]
fn round485_same_variant_ordering_is_not_reversed() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE s (t TEXT, b BOOLEAN, i INT)").unwrap();
    e.execute("INSERT INTO s VALUES ('b', true, 2), ('a', false, 1), ('c', true, 3)")
        .unwrap();
    assert_eq!(rows(&mut e, "SELECT t FROM s ORDER BY t"), "a;b;c");
    assert_eq!(rows(&mut e, "SELECT t FROM s ORDER BY t DESC"), "c;b;a");
    assert_eq!(rows(&mut e, "SELECT b::text AS x FROM s ORDER BY b"), "false;true;true");
    assert_eq!(rows(&mut e, "SELECT i FROM s ORDER BY i"), "1;2;3");
}

// ---- the projection buffer a duplicate row leaves behind ----

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d (a INT, b TEXT, u INT)").unwrap();
    // Rows 1-2 duplicate, row 3 differs in b only, row 4 in a only, row 5
    // repeats row 1 after two other shapes have used the buffer, rows 6-7
    // duplicate on a NULL, row 8 carries a NULL in the other column.
    e.execute(
        "INSERT INTO d VALUES (1, 'x', 10), (1, 'x', 11), (1, 'y', 12), (2, 'x', 13), \
         (1, 'x', 14), (NULL, 'z', 15), (NULL, 'z', 16), (2, NULL, 17)",
    )
    .unwrap();
    e
}

#[test]
fn round485_plain_projection_is_unchanged() {
    let mut e = seeded();
    assert_eq!(
        rows(&mut e, "SELECT a, b FROM d ORDER BY a NULLS LAST, b"),
        "1|x;1|x;1|x;1|y;2|x;2|NULL;NULL|z;NULL|z"
    );
}

#[test]
fn round485_distinct_two_columns_across_reuse() {
    let mut e = seeded();
    assert_eq!(
        rows(
            &mut e,
            "SELECT DISTINCT a, b FROM d ORDER BY a NULLS LAST, b NULLS LAST"
        ),
        "1|x;1|y;2|x;2|NULL;NULL|z"
    );
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM (SELECT DISTINCT a, b FROM d) s"),
        "5"
    );
}

#[test]
fn round485_distinct_over_unique_column_keeps_every_row() {
    // Every row survives the probe, so the scan takes the buffer each
    // time and allocates a fresh one — the opposite branch from the
    // duplicate-heavy shape above.
    let mut e = seeded();
    assert_eq!(
        rows(&mut e, "SELECT DISTINCT u FROM d ORDER BY u"),
        "10;11;12;13;14;15;16;17"
    );
}

#[test]
fn round485_distinct_with_limit_and_expression() {
    let mut e = seeded();
    assert_eq!(
        rows(&mut e, "SELECT DISTINCT a FROM d ORDER BY a NULLS LAST LIMIT 2"),
        "1;2"
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT DISTINCT b, a + 1 FROM d ORDER BY b NULLS LAST, 2 NULLS LAST"
        ),
        "x|2;x|3;y|2;z|NULL;NULL|3"
    );
}
