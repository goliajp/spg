//! read01 round 337 (V62) — the `::regclass` call form.
//!
//! `pg_get_viewdef('v')` worked while `pg_get_viewdef('v'::regclass)`
//! answered NULL — and `::regclass` is the form PG's own documentation and
//! pg_dump use. The value already carried the name through the cast;
//! the function simply only accepted `Text`.
//!
//! Measuring the family turned up a second, worse thing: `'nope'::regclass`
//! passed the name through as TEXT instead of failing. PG 18.4 says
//! `ERROR: relation "nope" does not exist` — and it says it at the CAST, so
//! `pg_get_viewdef('nope'::regclass)` never runs. Silently degrading to
//! text meant a catalog join written `WHERE conrelid = 'nope'::regclass`
//! matched nothing at all rather than reporting the typo.
//!
//! `to_regclass('nope')` is the spelling that answers NULL, and still does.

use spg_engine::Engine;
use spg_storage::Value;

fn first(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        spg_engine::QueryResult::Rows { rows, .. } => rows
            .first()
            .and_then(|r| r.values.first())
            .cloned()
            .map(Value::into_owned)
            .unwrap_or(Value::Null),
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(v) => panic!("{sql}: expected an error, got {v:?}"),
        Err(x) => format!("{x}"),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("CREATE VIEW vv AS SELECT id FROM t").unwrap();
    e
}

/// The worst of the family: a table holding rows reported **0 bytes** when
/// asked the documented way. Only bare TEXT was read; a `regclass` fell into
/// the "numeric oid, no reverse map" arm and answered 0.
#[test]
fn relation_size_reads_the_regclass_spelling() {
    let mut e = fixture();
    e.execute("INSERT INTO t VALUES (1),(2)").unwrap();
    let by_name = first(&mut e, "SELECT pg_relation_size('t')");
    assert!(
        matches!(by_name, Value::BigInt(n) if n > 0),
        "a table with rows is not zero bytes: {by_name:?}"
    );
    assert_eq!(first(&mut e, "SELECT pg_relation_size('t'::regclass)"), by_name);
    let total = first(&mut e, "SELECT pg_total_relation_size('t'::regclass)");
    assert!(matches!(total, Value::BigInt(n) if n > 0), "{total:?}");
}

/// An index and a sequence are relations too — both have a `pg_class` row,
/// so both answer to `::regclass`. Only tables and views resolved, which is
/// how `'ix'::regclass` came back as the bare text `ix`.
#[test]
fn an_index_and_a_sequence_are_relations() {
    let mut e = fixture();
    e.execute("CREATE INDEX ix ON t (id)").unwrap();
    e.execute("CREATE SEQUENCE sq").unwrap();
    for name in ["ix", "sq"] {
        let v = first(&mut e, &format!("SELECT '{name}'::regclass"));
        assert!(matches!(v, Value::RegClass(_, _)), "{name}: {v:?}");
        assert_eq!(
            first(&mut e, &format!("SELECT '{name}'::regclass::text")),
            Value::text(name),
        );
    }
    // …and the index form of pg_get_indexdef agrees with the bare name.
    assert_eq!(
        first(&mut e, "SELECT pg_get_indexdef('ix'::regclass)"),
        first(&mut e, "SELECT pg_get_indexdef('ix')"),
    );
}

/// The canonical spelling works, and agrees with the bare name.
#[test]
fn pg_get_viewdef_accepts_a_regclass() {
    let mut e = fixture();
    let by_name = first(&mut e, "SELECT pg_get_viewdef('vv')");
    let by_regclass = first(&mut e, "SELECT pg_get_viewdef('vv'::regclass)");
    assert_eq!(by_regclass, by_name);
    assert!(matches!(by_name, Value::Text(_)), "{by_name:?}");
    // …including the schema-qualified spelling pg_dump emits.
    assert_eq!(
        first(&mut e, "SELECT pg_get_viewdef('public.vv'::regclass)"),
        by_name,
    );
}

/// A name that is no relation is PG's error, raised by the cast.
#[test]
fn a_missing_relation_is_an_error_not_a_text_passthrough() {
    let mut e = fixture();
    assert_eq!(
        err(&mut e, "SELECT 'nope'::regclass"),
        "eval: type mismatch: relation \"nope\" does not exist",
    );
    // And so the function call on it never runs.
    assert!(
        err(&mut e, "SELECT pg_get_viewdef('nope'::regclass)")
            .contains("relation \"nope\" does not exist"),
    );
}

/// `to_regclass` is the NULL-returning spelling — that difference is the
/// whole reason PG has both.
#[test]
fn to_regclass_still_answers_null_for_a_miss() {
    let mut e = fixture();
    assert_eq!(first(&mut e, "SELECT to_regclass('nope')"), Value::Null);
}

/// A real relation still resolves to the dual oid+name shape catalog joins
/// depend on.
#[test]
fn a_real_relation_keeps_its_oid() {
    let mut e = fixture();
    assert!(
        matches!(first(&mut e, "SELECT 't'::regclass"), Value::RegClass(_, _)),
        "a resolvable name keeps the oid half"
    );
    assert_eq!(
        first(&mut e, "SELECT 't'::regclass::text"),
        Value::text("t"),
    );
}
