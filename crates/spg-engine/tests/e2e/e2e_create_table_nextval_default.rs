//! v7.38.19 — `CREATE TABLE t (id bigint DEFAULT nextval('s'), …)`
//! could not insert a row.
//!
//!     ERROR:  nextval() requires a sequence resolver (read-only context)
//!
//! PostgreSQL 18.4 inserts. The same column reached by the OTHER
//! spelling worked here all along:
//!
//! ```text
//!   CREATE TABLE … DEFAULT nextval('zs')             ERROR
//!   CREATE TABLE … DEFAULT nextval('zs'::regclass)   ERROR
//!   ALTER … SET DEFAULT nextval('zs')                INSERT 0 1
//!   ALTER … SET DEFAULT nextval('zs'::regclass)      INSERT 0 1
//! ```
//!
//! The ALTER form has been recognised since v7.22, because that is what
//! `pg_dump` emits for a serial column and imports were losing their
//! numbering. It lowers to the auto-increment marker, which the INSERT
//! path fills from the table. The CREATE TABLE form stored the same
//! expression as text to be re-parsed and evaluated per INSERT -- and
//! the context it is evaluated in has no way to advance a sequence,
//! because advancing one needs a mutable catalog the row-level
//! evaluator does not hold.
//!
//! Two spellings of one column definition disagreed about whether the
//! column worked.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                spg_storage::Value::Text(t) => t.to_string(),
                spg_storage::Value::Null => "<NULL>".into(),
                other => format!("{other:?}"),
            })
            .collect(),
        other => panic!("expected rows from {sql}, got {other:?}"),
    }
}

fn seeded(default_expr: &str) -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE zs").unwrap();
    e.execute(&format!(
        "CREATE TABLE z (id bigint DEFAULT {default_expr}, k text)"
    ))
    .unwrap();
    e
}

#[test]
fn a_create_table_nextval_default_fills_the_column() {
    for spelling in ["nextval('zs')", "nextval('zs'::regclass)"] {
        let mut e = seeded(spelling);
        for k in ["a", "b", "c"] {
            e.execute(&format!("INSERT INTO z (k) VALUES ('{k}')"))
                .unwrap_or_else(|err| panic!("{spelling}: {err:?}"));
        }
        assert_eq!(
            rows(&mut e, "SELECT id FROM z ORDER BY id"),
            ["BigInt(1)", "BigInt(2)", "BigInt(3)"],
            "{spelling}"
        );
    }
}

/// An explicit value is accepted and kept, and the numbering carries on
/// ABOVE it.
///
/// This is where we differ from PostgreSQL, and the difference is older
/// than this change -- it belongs to the auto-increment machinery the
/// ALTER spelling has used since v7.22, and this only routes a second
/// spelling into it. Measured against PostgreSQL 18.4 on the same three
/// inserts:
///
/// ```text
///   PostgreSQL 18.4   1, 2, 50
///   SPG               1, 50, 51
/// ```
///
/// PostgreSQL's sequence is a counter that knows nothing about the
/// table, so an explicit 50 does not move it and the next row is 2 --
/// which means the sequence will eventually reach 50 and collide. Ours
/// is the table's maximum plus one, so it never hands out a value the
/// table already holds, and never goes back. Recorded as RD-12 rather
/// than quietly pinned as correct.
#[test]
fn an_explicit_value_is_kept_and_the_numbering_follows_it() {
    let mut e = seeded("nextval('zs')");
    e.execute("INSERT INTO z (k) VALUES ('a')").unwrap();
    e.execute("INSERT INTO z (id, k) VALUES (50, 'b')").unwrap();
    e.execute("INSERT INTO z (k) VALUES ('c')").unwrap();
    assert_eq!(
        rows(&mut e, "SELECT id FROM z ORDER BY id"),
        ["BigInt(1)", "BigInt(50)", "BigInt(51)"]
    );
}

/// The two spellings must agree, which is the whole complaint. Built
/// the other way, the same table answers the same way.
#[test]
fn the_alter_spelling_agrees() {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE zs").unwrap();
    e.execute("CREATE TABLE z (id bigint, k text)").unwrap();
    e.execute("ALTER TABLE z ALTER COLUMN id SET DEFAULT nextval('zs')")
        .unwrap();
    for k in ["a", "b", "c"] {
        e.execute(&format!("INSERT INTO z (k) VALUES ('{k}')"))
            .unwrap();
    }
    assert_eq!(
        rows(&mut e, "SELECT id FROM z ORDER BY id"),
        ["BigInt(1)", "BigInt(2)", "BigInt(3)"]
    );
}

/// A non-integer column cannot be numbered, and must say so rather than
/// accept the definition and fail at the first INSERT -- which is the
/// shape of the defect being fixed.
#[test]
fn a_nextval_default_on_a_text_column_is_refused_at_definition_time() {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE zs").unwrap();
    let r = e.execute("CREATE TABLE z (id text DEFAULT nextval('zs'), k text)");
    assert!(
        r.is_err(),
        "a text column cannot carry a sequence default: {r:?}"
    );
}

/// Other expression defaults are untouched: they keep the re-parse path,
/// which is right for them.
#[test]
fn other_expression_defaults_still_work() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE w (id bigint DEFAULT 7, t timestamp DEFAULT now(), k text)")
        .unwrap();
    e.execute("INSERT INTO w (k) VALUES ('a')").unwrap();
    assert_eq!(rows(&mut e, "SELECT id FROM w"), ["BigInt(7)"]);
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM w WHERE t IS NOT NULL"),
        ["BigInt(1)"]
    );
}

/// The catalog text a schema-diff tool compares. PostgreSQL 18.4 prints
///
/// ```text
///   nextval('zs'::regclass)
/// ```
///
/// and we printed `nextval(('zs')::regclass)` — the generic deparser
/// parenthesises a cast. It re-parsed here and our own dump round-trip
/// was a fixed point, so it never broke anything of ours; it broke the
/// comparison with theirs, which is the bar.
#[test]
fn the_catalog_text_matches_postgresql() {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE zs").unwrap();
    e.execute("CREATE TABLE z (id bigint DEFAULT nextval('zs'::regclass), k text)")
        .unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT column_default FROM information_schema.columns \
             WHERE table_name = 'z' AND column_name = 'id'"
        ),
        ["nextval('zs'::regclass)"]
    );
    // The untyped spelling stays untyped, as PostgreSQL leaves it.
    let mut e2 = Engine::new();
    e2.execute("CREATE SEQUENCE zs").unwrap();
    e2.execute("CREATE TABLE z (id bigint DEFAULT nextval('zs'), k text)")
        .unwrap();
    assert_eq!(
        rows(
            &mut e2,
            "SELECT column_default FROM information_schema.columns \
             WHERE table_name = 'z' AND column_name = 'id'"
        ),
        ["nextval('zs')"]
    );
}
