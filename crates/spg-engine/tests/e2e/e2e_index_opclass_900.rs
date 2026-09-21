//! 9.0.0 — `pg_index.indclass` was a row of zeros, and every operator
//! class carried a made-up oid.
//!
//! `indclass` names the operator class each key part compares with. A
//! client reads it by joining to `pg_opclass` — that is how `\d` prints
//! `text_pattern_ops`, and how a schema-diff tool decides two indexes
//! differ. SPG answered `0` for every part, and the `pg_opclass` rows it
//! did publish were numbered `20000 + position`, so the join found
//! nothing on either side of the gap.
//!
//! Measured on PostgreSQL 18.6:
//!
//! ```text
//!   CREATE TABLE b8t(a text, b int, c int[], d numeric, e timestamptz);
//!   b8i1 (a, b)                  indclass = 3126 1978
//!   b8i2 USING gin (c)           indclass = 10064     -- gin/anyarray
//!   b8i3 USING brin (b, d)       indclass = 10104 10153
//!   b8i4 USING hash (a)          indclass = 10037
//!   b8i5 (lower(a), e)           indclass = 3126 3127
//!   b9i1 (t text_pattern_ops)    indclass = 4217
//!   b9i2 USING gin (j jsonb_path_ops)  indclass = 10091
//! ```
//!
//! Two things had to be true for those to come out. The class a part
//! uses is the one the statement NAMED, else the default for its type
//! under that access method — both read off the live catalog, since an
//! oid is data, not an algorithm. And the access method has to be the
//! one asked for: `USING gin (int_array)` is a shape SPG's GIN kinds do
//! not cover, so it builds a B-tree, and before this it also REPORTED a
//! B-tree — which put `btree/anyarray` (10000) where PostgreSQL has
//! `gin/anyarray` (10064).

use spg_engine::{Engine, QueryResult};

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    rows.into_iter()
        .map(|r| spg_engine::eval::value_to_text(r.values.first().expect("one column")))
        .collect()
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE TABLE b8t (a text, b int, c int[], d numeric, e timestamptz)",
        "CREATE INDEX b8i1 ON b8t (a, b)",
        "CREATE INDEX b8i2 ON b8t USING gin (c)",
        "CREATE INDEX b8i3 ON b8t USING brin (b, d)",
        "CREATE INDEX b8i4 ON b8t USING hash (a)",
        "CREATE INDEX b8i5 ON b8t (lower(a), e)",
        "CREATE TABLE b9t (t text, j jsonb)",
        "CREATE INDEX b9i1 ON b9t (t text_pattern_ops)",
        "CREATE INDEX b9i2 ON b9t USING gin (j jsonb_path_ops)",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    e
}

fn indclass(e: &mut Engine, index: &str) -> String {
    let sql = format!(
        "SELECT i.indclass FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
         WHERE c.relname = '{index}'"
    );
    col(e, &sql).into_iter().next().expect("one row")
}

#[test]
fn every_key_part_names_the_class_postgresql_names() {
    let mut e = seeded();
    assert_eq!(indclass(&mut e, "b8i1"), "3126 1978");
    assert_eq!(indclass(&mut e, "b8i2"), "10064");
    assert_eq!(indclass(&mut e, "b8i3"), "10104 10153");
    assert_eq!(indclass(&mut e, "b8i4"), "10037");
    assert_eq!(indclass(&mut e, "b8i5"), "3126 3127");
}

#[test]
fn a_named_operator_class_wins_over_the_type_default() {
    let mut e = seeded();
    assert_eq!(indclass(&mut e, "b9i1"), "4217");
    assert_eq!(indclass(&mut e, "b9i2"), "10091");
}

#[test]
fn the_oids_resolve_against_pg_opclass() {
    // The point of the numbers: a client joins them. `0` joined to
    // nothing, and so did `20000 + position`.
    let mut e = seeded();
    assert_eq!(
        col(
            &mut e,
            "SELECT o.opcname FROM pg_opclass o WHERE o.oid IN (3126, 1978, 10064, 4217, 10091) \
             ORDER BY o.opcname"
        ),
        vec![
            "array_ops".to_string(),
            "int4_ops".to_string(),
            "jsonb_path_ops".to_string(),
            "text_ops".to_string(),
            "text_pattern_ops".to_string(),
        ]
    );
}

#[test]
fn the_access_method_asked_for_is_the_one_reported() {
    // `USING gin (int[])` is not a shape SPG's GIN covers, so it builds a
    // B-tree. What it reports is still `gin` — the name is what a dump
    // round-trips, and it is what picks the opclass.
    let mut e = seeded();
    assert_eq!(
        col(
            &mut e,
            "SELECT a.amname FROM pg_class c JOIN pg_am a ON a.oid = c.relam WHERE c.relname='b8i2'"
        ),
        vec!["gin".to_string()]
    );
}

#[test]
fn a_vector_column_spreads_into_an_array_of_any_element_type() {
    // `indkey` and `indclass` are `int2vector` / `oidvector`, and PG
    // converts either to the matching array. Both spellings a client
    // reaches for — the parser's own `::int[]` and the generic
    // `::oid[]` — went through different code, and only one of them
    // knew what a vector was.
    let mut e = seeded();
    assert_eq!(
        col(
            &mut e,
            "SELECT array_length(i.indkey::int[], 1) FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = 'b8i1'"
        ),
        vec!["2".to_string()]
    );
    assert_eq!(
        col(
            &mut e,
            "SELECT array_to_string(i.indclass::text[], ',') FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = 'b8i1'"
        ),
        vec!["3126,1978".to_string()]
    );
    // And `oid[]` as a target for an array that is already one: PG takes
    // both widths, SPG took neither.
    assert_eq!(
        col(&mut e, "SELECT ('{1,2}'::int[])::oid[]"),
        vec!["{1,2}".to_string()]
    );
    assert_eq!(
        col(&mut e, "SELECT ('{3,4}'::bigint[])::oid[]"),
        vec!["{3,4}".to_string()]
    );
}

#[test]
fn indclass_casts_to_an_array_and_joins() {
    // The whole reason the column exists, written the way a schema tool
    // writes it. `indclass::oid[]` used to answer "cannot cast
    // oidvector to oid[]".
    let mut e = seeded();
    assert_eq!(
        col(
            &mut e,
            "SELECT o.opcname FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
             CROSS JOIN LATERAL unnest(i.indclass::oid[]) AS k(cls) \
             JOIN pg_opclass o ON o.oid = k.cls WHERE c.relname = 'b8i1'"
        ),
        vec!["text_ops".to_string(), "int4_ops".to_string()]
    );
}
