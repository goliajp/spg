//! v7.39 (round 258) — enum type identity, swept 58 cases against live
//! PG18.4 (2026-07-19). Comparison, `enum_range` / `enum_first` /
//! `enum_last`, `ALTER TYPE … ADD VALUE [BEFORE …]`, ORDER BY and
//! min/max over a real enum COLUMN all matched already. The gaps shared
//! one root cause: an enum value travels as `Value::Text` (its label),
//! so the identity has to ride the SCHEMA, and two places dropped it:
//!
//!   * `pg_typeof` answered `text` for every enum — it names the type
//!     from the VALUE. It now resolves the expression statically
//!     (column or cast), the same discipline round 253 used for
//!     EXTRACT's type name, and reports `mood[]` for an enum array.
//!   * A derived table forgot the enum: `FROM (VALUES ('happy'::mood),
//!     …) t(m)` lowers to constant SELECTs, and a user enum has no
//!     `DataType` of its own, so `describe_expr` cannot type the cast
//!     and the projected item fell to the untyped default — text. The
//!     outer ORDER BY / min / max / array_agg then sorted by LABEL
//!     instead of member order: silently wrong rows, not an error.
//!
//! Also fixed here: round 257's DISTINCT-aggregate sort used the
//! generic value comparison, so `array_agg(DISTINCT m)` over an enum
//! column sorted by text — a regression that round introduced. The
//! aggregate's own `enum_labels` were derived only for min/max; they
//! now cover DISTINCT aggregates too.

use spg_engine::{Engine, QueryResult};

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TYPE mp AS ENUM ('sad','ok','happy')")
        .unwrap();
    e.execute("CREATE TABLE ep (id int, m mp)").unwrap();
    e.execute("INSERT INTO ep VALUES (1,'happy'),(2,'sad'),(3,'ok'),(4,'ok')")
        .unwrap();
    e
}

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            // psql -tA prints booleans t / f; speak the oracle's dialect.
            spg_storage::Value::Bool(b) => String::from(if *b { "t" } else { "f" }),
            other => spg_engine::eval::value_to_text(other),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn pg_typeof_names_the_enum_not_its_label_type() {
    let mut e = seeded();
    assert_eq!(one(&mut e, "SELECT pg_typeof(m) FROM ep LIMIT 1"), "mp");
    assert_eq!(one(&mut e, "SELECT pg_typeof('ok'::mp)"), "mp");
    assert_eq!(one(&mut e, "SELECT pg_typeof(ARRAY['ok'::mp])"), "mp[]");
    // Through a derived table too.
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(x) FROM (SELECT 'ok'::mp AS x) t"),
        "mp"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_typeof(m) FROM (VALUES ('happy'::mp)) t(m)"
        ),
        "mp"
    );
}

#[test]
fn a_derived_table_keeps_member_order() {
    let mut e = seeded();
    // These all sorted by LABEL before: happy < ok < sad alphabetically,
    // where the member order is sad < ok < happy.
    assert_eq!(
        col(
            &mut e,
            "SELECT m FROM (VALUES ('happy'::mp),('sad'::mp)) t(m) ORDER BY m"
        ),
        ["sad", "happy"]
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT max(m) FROM (VALUES ('sad'::mp),('happy'::mp)) t(m)"
        ),
        "happy"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT min(m) FROM (VALUES ('ok'::mp),('happy'::mp)) t(m)"
        ),
        "ok"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT array_agg(m ORDER BY m) FROM (VALUES ('happy'::mp),('sad'::mp),('ok'::mp)) t(m)"
        ),
        "{sad,ok,happy}"
    );
    // A plain UNION ALL derived table is the same shape.
    assert_eq!(
        col(
            &mut e,
            "SELECT x FROM (SELECT 'happy'::mp AS x UNION ALL SELECT 'sad'::mp) t ORDER BY x"
        ),
        ["sad", "happy"]
    );
}

#[test]
fn distinct_aggregates_sort_by_member_order() {
    let mut e = seeded();
    // Round 257's DISTINCT sort regressed this to text order.
    assert_eq!(
        one(&mut e, "SELECT array_agg(DISTINCT m) FROM ep"),
        "{sad,ok,happy}"
    );
    // A DISTINCT aggregate over the LABELS is a text sort, as PG has it.
    assert_eq!(
        one(&mut e, "SELECT string_agg(DISTINCT m::text, ',') FROM ep"),
        "happy,ok,sad"
    );
}

#[test]
fn the_enum_core_is_unchanged() {
    let mut e = seeded();
    for (sql, want) in [
        ("SELECT 'sad'::mp < 'happy'::mp", "t"),
        ("SELECT 'happy'::mp > 'ok'::mp", "t"),
        ("SELECT enum_range(NULL::mp)", "{sad,ok,happy}"),
        ("SELECT enum_first(NULL::mp)", "sad"),
        ("SELECT enum_last(NULL::mp)", "happy"),
        ("SELECT 'ok'::mp::text", "ok"),
        ("SELECT max(m) FROM ep", "happy"),
        ("SELECT min(m) FROM ep", "sad"),
        (
            "SELECT array_agg(m ORDER BY m) FROM ep",
            "{sad,ok,ok,happy}",
        ),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
    assert_eq!(
        col(&mut e, "SELECT m FROM ep ORDER BY m"),
        ["sad", "ok", "ok", "happy"]
    );
    // An unknown label is refused.
    assert!(e.execute("SELECT 'nope'::mp").is_err());
}
