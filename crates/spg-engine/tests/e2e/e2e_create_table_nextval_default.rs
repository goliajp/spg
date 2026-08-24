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

/// The next value for a `serial` column must not depend on how many
/// rows the table holds.
///
/// It did: `next_auto_value` walked every row to find the maximum, so
/// one INSERT into a 200,000-row table cost 3.666 ms against
/// PostgreSQL 18's flat 1.375, and the gap grew with the table —
/// 1.831 at a thousand rows, 2.703 at fifty thousand. An ingest
/// workload got slower the longer it ran. It is 1.106 now, against
/// their 1.075.
///
/// The assertion is on the DECISION, because nothing else can see it.
/// The first version of this test watched `seq_scan` and asserted it did
/// not move — it does not move either way, since that counter is for
/// query-level scans and this walk is inside the insert path. Removing
/// the fix left the test green, which is how it was found; a test whose
/// negative control passes is checking nothing.
///
/// The two paths cannot be told apart by their ANSWERS — measured, they
/// agree, including after a delete (see the case below) — so the
/// decision is asked directly, from the one place that makes it.
#[test]
fn the_next_serial_value_comes_from_the_index_not_the_rows() {
    use spg_storage::Table;

    fn table_of(e: &Engine, name: &str) -> Table {
        e.catalog()
            .get(name)
            .unwrap_or_else(|| panic!("no table {name}"))
            .clone()
    }

    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE zs").unwrap();
    e.execute("CREATE TABLE indexed (id bigint PRIMARY KEY DEFAULT nextval('zs'), k text)")
        .unwrap();
    e.execute("CREATE TABLE plain (id bigserial, k text)")
        .unwrap();
    for i in 0..50 {
        e.execute(&format!("INSERT INTO indexed (k) VALUES ('r{i}')"))
            .unwrap();
        e.execute(&format!("INSERT INTO plain (k) VALUES ('r{i}')"))
            .unwrap();
    }

    let indexed = table_of(&e, "indexed");
    let plain = table_of(&e, "plain");
    assert_eq!(
        indexed.auto_value_from_index(0),
        Some(51),
        "a PRIMARY KEY is an index, and its largest key is the answer"
    );
    assert_eq!(
        plain.auto_value_from_index(0),
        None,
        "no index on the column, so there is nothing to descend"
    );
    // Whichever path a table takes, the number is the same.
    assert_eq!(indexed.next_auto_value(0), Some(51));
    assert_eq!(plain.next_auto_value(0), Some(51));

    e.execute("INSERT INTO indexed (k) VALUES ('one more')")
        .unwrap();
    assert_eq!(rows(&mut e, "SELECT max(id) FROM indexed"), ["BigInt(51)"]);
    assert_eq!(rows(&mut e, "SELECT count(*) FROM indexed"), ["BigInt(51)"]);
}

/// Deleting the highest row and inserting again does not reuse its id —
/// on either path, and on PostgreSQL 18.4, which all three answer
/// `1,2,3,4,6`. A deleted row leaves a version behind that the tree and
/// the scan both still see, which is what makes the two paths
/// interchangeable rather than merely similar.
#[test]
fn a_deleted_top_id_is_not_handed_out_again() {
    for ddl in [
        "CREATE TABLE dz (id bigserial PRIMARY KEY, k text)",
        "CREATE TABLE dz (id bigserial, k text)",
    ] {
        let mut e = Engine::new();
        e.execute(ddl).unwrap();
        for i in 0..5 {
            e.execute(&format!("INSERT INTO dz (k) VALUES ('r{i}')"))
                .unwrap();
        }
        e.execute("DELETE FROM dz WHERE id = 5").unwrap();
        e.execute("INSERT INTO dz (k) VALUES ('after')").unwrap();
        assert_eq!(
            rows(&mut e, "SELECT id FROM dz ORDER BY id"),
            [
                "BigInt(1)",
                "BigInt(2)",
                "BigInt(3)",
                "BigInt(4)",
                "BigInt(6)"
            ],
            "{ddl}"
        );
    }
}
