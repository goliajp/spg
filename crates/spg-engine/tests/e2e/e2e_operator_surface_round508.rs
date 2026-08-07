//! v7.39 (round 508) — the operators PG18 has that SPG did not.
//!
//! r507 found SPG had no unary `+` at all and nothing caught it, because the
//! operator surface had never been swept. `scripts/operator-surface-diff.py`
//! sweeps it generatively — the list comes from PG's own `pg_operator`, not
//! from memory — and found 33 forms in 8 families that PG resolves and SPG
//! refused:
//!
//! | family          | forms | meaning                                   |
//! |-----------------|-------|-------------------------------------------|
//! | `@` prefix      | 6     | absolute value                            |
//! | `?#`            | 7     | do these shapes intersect                 |
//! | `<^` `>^`       | 4     | strictly below / strictly above           |
//! | `~<~` family    | 4     | `text_pattern_ops` byte comparisons       |
//! | `#` prefix      | 2     | vertex count                              |
//! | `@-@` prefix    | 2     | length                                    |
//! | `@@@`           | 2     | the old spelling of `@@`                   |
//! | `@@` / `?-`     | 6     | already implemented, but NULL fell through |
//!
//! The last family was not a missing operator: `@@ NULL::box` desugars to
//! `center(NULL)`, and the guard on the geometric functions only matched a
//! real geometric value, so a NULL operand reached the unknown-function arm
//! and answered "function center(unknown) does not exist" where PG answers
//! NULL. These functions are strict; now they say so.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    Engine::new()
}

/// Every value of the first row, joined — the panels below check several
/// operators per statement, exactly as they were measured.
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

/// `@ x` is the absolute value, and it keeps the operand's type.
#[test]
fn round508_at_prefix_is_absolute_value() {
    let mut e = engine();
    assert_eq!(
        text(&mut e, "SELECT @ -5, @ -5.5, @ (-3)::float8, @ 7"),
        "5|5.5|3|7"
    );
    assert_eq!(text(&mut e, "SELECT pg_typeof(@ -5)"), "integer");
    assert_eq!(text(&mut e, "SELECT @ NULL::int4"), "NULL");
}

/// `# p` counts vertices; `@-@ p` measures length. A closed path's length is
/// its perimeter, which is why the two-point path below measures 10 and the
/// same-shaped lseg measures 5.
#[test]
fn round508_hash_and_at_minus_at_are_npoints_and_length() {
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT # path '((0,0),(1,1),(2,0))', # polygon '((0,0),(1,1),(2,0))'"
        ),
        "3|3"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT @-@ lseg '((0,0),(3,4))', @-@ path '((0,0),(3,4))'"
        ),
        "5|10"
    );
}

/// `<^` / `>^` compare vertical position. On boxes it is the two extents
/// that are compared, so one box is below another only when it is ENTIRELY
/// below it.
#[test]
fn round508_below_and_above() {
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT point '(0,0)' <^ point '(0,1)', point '(0,1)' <^ point '(0,0)', \
             point '(0,1)' >^ point '(0,0)'"
        ),
        "true|false|true"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT box '((0,0),(1,1))' <^ box '((0,2),(1,3))', \
             box '((0,2),(1,3))' >^ box '((0,0),(1,1))'"
        ),
        "true|true"
    );
}

/// `?#` — do the two shapes meet.
#[test]
fn round508_intersects() {
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT box '((0,0),(2,2))' ?# box '((1,1),(3,3))', \
             box '((0,0),(1,1))' ?# box '((5,5),(6,6))'"
        ),
        "true|false"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT lseg '((0,0),(2,2))' ?# lseg '((0,2),(2,0))', \
             lseg '((0,0),(1,1))' ?# lseg '((5,5),(6,6))'"
        ),
        "true|false"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT path '((0,0),(2,2))' ?# path '((0,2),(2,0))'"
        ),
        "true"
    );
}

/// The `text_pattern_ops` comparisons order by BYTES, not by collation.
/// That is the whole point of the family — it is what lets a LIKE prefix
/// use an index — and pg_dump writes it into index definitions, so a dump
/// of an ordinary database would not have restored without them.
#[test]
fn round508_pattern_ops_compare_bytes() {
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT 'a' ~<~ 'b', 'b' ~<~ 'a', 'a' ~<=~ 'a', 'b' ~>~ 'a', 'a' ~>=~ 'a'"
        ),
        "true|false|true|true|true"
    );
    // Uppercase sorts before lowercase in byte order, whatever the
    // collation would say.
    assert_eq!(text(&mut e, "SELECT 'A' ~<~ 'a'"), "true");
}

/// `@@@` is the old spelling of `@@` and means exactly it.
#[test]
fn round508_at_at_at_is_the_old_spelling_of_at_at() {
    let mut e = engine();
    assert_eq!(
        text(&mut e, "SELECT 'cat'::tsvector @@@ 'cat'::tsquery"),
        text(&mut e, "SELECT 'cat'::tsvector @@ 'cat'::tsquery")
    );
    assert_eq!(
        text(&mut e, "SELECT 'cat'::tsquery @@@ 'cat'::tsvector"),
        "true"
    );
}

/// The geometric functions are strict, so the prefix operators that desugar
/// to them answer NULL rather than "function center(unknown) does not
/// exist".
#[test]
fn round508_geometric_prefixes_are_strict() {
    let mut e = engine();
    for sql in [
        "SELECT @@ NULL::box",
        "SELECT ?- NULL::lseg",
        "SELECT ?| NULL::lseg",
        "SELECT # NULL::path",
        "SELECT @-@ NULL::lseg",
    ] {
        assert_eq!(text(&mut e, sql), "NULL", "{sql}");
    }
}
