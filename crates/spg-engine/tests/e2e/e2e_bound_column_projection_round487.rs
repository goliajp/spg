//! read01 round 487 — a bare column in the SELECT list binds once.
//!
//! Projecting `g` used to cost, per row: a memo lookup for "does this
//! expression contain a subquery", an un-memoised `expr_may_use_in_set`
//! tree walk, `eval_expr`'s dispatch, and then `resolve_column`, which
//! finds the column by scanning the schema and comparing NAMES. On
//! `SELECT g FROM h` that chain was 19 % of self time for what is one
//! cell read.
//!
//! The position is a per-query fact, so it is bound once via
//! `compile_column_pos` — the Step VM's resolver, which returns None for
//! anything that would reach an error, an ambiguity, or a miss, so those
//! keep going the interpreter's way and keep its exact message.
//!
//! These pin the cases where "just read the cell" would be WRONG: a
//! composite column (must be rehydrated from stored JSON), a whole-row
//! reference (a name that is the alias, not a column), an ambiguous name
//! across a join, and a name that does not exist. Plus the ordinary
//! spellings, since binding to the wrong position is silent.
//!
//! Expectations are PG18's, read off `psql -tA`.

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

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TYPE pt AS (a INT, b TEXT)").unwrap();
    e.execute("CREATE TABLE t (id INT, g INT, s TEXT, p pt)").unwrap();
    e.execute(
        "INSERT INTO t VALUES (1, 10, 'x', ROW(1,'one')), (2, NULL, NULL, NULL), \
         (3, 30, 'z', ROW(3,'three'))",
    )
    .unwrap();
    e.execute("CREATE TABLE u (id INT, g INT)").unwrap();
    e.execute("INSERT INTO u VALUES (1, 100), (2, 200)").unwrap();
    e
}

#[test]
fn round487_every_spelling_binds_the_same_column() {
    let mut e = seeded();
    // PG18: 10, NULL, 30 for all three spellings.
    assert_eq!(rows(&mut e, "SELECT g FROM t ORDER BY id"), "10;NULL;30");
    assert_eq!(rows(&mut e, "SELECT x.g FROM t x ORDER BY x.id"), "10;NULL;30");
    assert_eq!(rows(&mut e, "SELECT g FROM t x ORDER BY id"), "10;NULL;30");
}

#[test]
fn round487_several_columns_keep_their_own_positions() {
    // Binding to the wrong position is silent, so check a row whose
    // columns are distinguishable and one that is all NULL.
    let mut e = seeded();
    assert_eq!(
        rows(&mut e, "SELECT id, g, s FROM t ORDER BY id"),
        "1|10|x;2|NULL|NULL;3|30|z"
    );
    // Reversed order, and a column repeated.
    assert_eq!(
        rows(&mut e, "SELECT s, g, id, g FROM t ORDER BY id"),
        "x|10|1|10;NULL|NULL|2|NULL;z|30|3|30"
    );
}

#[test]
fn round487_text_cells_survive_the_read() {
    let mut e = seeded();
    assert_eq!(rows(&mut e, "SELECT s FROM t ORDER BY id"), "x;NULL;z");
}

#[test]
fn round487_composite_column_still_rehydrates() {
    // A composite is stored as JSON and has to be rebuilt into a
    // `Value::Composite`; reading the cell straight off the row would
    // hand back the raw JSON. The binding excludes it for exactly the
    // reason the Step VM does.
    let mut e = seeded();
    assert_eq!(
        rows(&mut e, "SELECT p::text AS pt FROM t ORDER BY id"),
        "(1,one);NULL;(3,three)"
    );
    assert_eq!(rows(&mut e, "SELECT (p).b AS f FROM t ORDER BY id"), "one;NULL;three");
}

#[test]
fn round487_whole_row_reference_is_not_a_column() {
    // `t` is the alias, not a column: PG answers the whole row as a
    // composite. The binder must not find a position for it.
    let mut e = seeded();
    assert_eq!(
        rows(&mut e, "SELECT t::text AS w FROM t ORDER BY id"),
        "(1,10,x,\"(1,one)\");(2,,,);(3,30,z,\"(3,three)\")"
    );
}

#[test]
fn round487_unknown_and_ambiguous_names_still_error() {
    let mut e = seeded();
    let unknown = e.execute("SELECT nope FROM t");
    assert!(unknown.is_err(), "unknown column -> {unknown:?}");
    let ambiguous = e.execute("SELECT g FROM t a JOIN u b ON a.id = b.id");
    assert!(ambiguous.is_err(), "ambiguous column -> {ambiguous:?}");
}
