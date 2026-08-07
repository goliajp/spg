//! read01 round 338 (V64) — one relation, one oid.
//!
//! Every kind of relation numbered itself wherever it was synthesised, and
//! the numbers disagreed. A sequence was 300_001 in `pg_class` but 32_768
//! in `pg_sequence`, so PG's canonical
//! `pg_class JOIN pg_sequence ON oid = seqrelid` returned nothing — and
//! 32_768 was simultaneously the *view* band, so a view and a sequence
//! answered to the same oid.
//!
//! Measuring that turned up the bigger hole underneath: `pg_class` had
//! **no view rows at all**, `pg_attribute` had no view columns, and a
//! MATERIALIZED VIEW — a real table underneath in SPG — reported
//! `relkind = 'r'`, so `WHERE relkind = 'm'` listed none of them and a
//! migration tool would recreate one as a plain table.
//!
//! PG 18.4 measured for the view row: relam 0, relfilenode 0, relpages 0,
//! reltuples -1, relhasrules TRUE (its _RETURN rule), relreplident 'n'.

use spg_engine::Engine;
use spg_storage::Value;

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        spg_engine::QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| r.values.iter().cloned().map(Value::into_owned).collect())
            .collect(),
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

fn one(e: &mut Engine, sql: &str) -> Value<'static> {
    rows(e, sql)
        .first()
        .and_then(|r| r.first())
        .cloned()
        .unwrap_or(Value::Null)
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT, v TEXT)").unwrap();
    e.execute("CREATE INDEX ix ON t (id)").unwrap();
    e.execute("CREATE SEQUENCE sq").unwrap();
    e.execute("CREATE VIEW vv AS SELECT id FROM t").unwrap();
    e.execute("CREATE MATERIALIZED VIEW mv AS SELECT id FROM t")
        .unwrap();
    e
}

/// The guarantee the whole catalog rests on: what `::regclass` resolves to
/// is what `pg_class.oid` holds — for every kind, not only for tables.
#[test]
fn pg_class_oid_agrees_with_regclass_for_every_kind() {
    let mut e = fixture();
    for name in ["t", "vv", "mv", "ix", "sq"] {
        let found = one(
            &mut e,
            &format!("SELECT relname FROM pg_class WHERE oid = '{name}'::regclass"),
        );
        assert_eq!(found, Value::text(name), "pg_class row for {name}");
    }
    // …and no two kinds share an oid.
    let all = rows(&mut e, "SELECT oid FROM pg_class");
    let mut oids: Vec<_> = all.iter().map(|r| format!("{:?}", r[0])).collect();
    let before = oids.len();
    oids.sort();
    oids.dedup();
    assert_eq!(oids.len(), before, "two relations share an oid");
}

/// PG's canonical sequence join. It returned zero rows.
#[test]
fn the_pg_sequence_join_resolves() {
    let mut e = fixture();
    let r = rows(
        &mut e,
        "SELECT c.relname, s.seqstart FROM pg_class c \
         JOIN pg_sequence s ON c.oid = s.seqrelid",
    );
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0][0], Value::text("sq"));
    assert_eq!(r[0][1], Value::BigInt(1));
}

/// A view is a relation: it has a pg_class row, and PG's values for it.
#[test]
fn a_view_has_a_pg_class_row() {
    let mut e = fixture();
    let r = rows(
        &mut e,
        "SELECT relkind, relnatts, relhasrules, relam, relfilenode, \
                relpages, reltuples, relreplident, relhasindex \
           FROM pg_class WHERE relname = 'vv'",
    );
    assert_eq!(r.len(), 1, "pg_class had no row for the view");
    assert_eq!(
        r[0],
        alloc_row(&[
            Value::text("v"),
            Value::SmallInt(1),
            Value::Bool(true),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::Int(0),
            Value::Float(-1.0),
            Value::text("n"),
            Value::Bool(false),
        ]),
    );
}

fn alloc_row(v: &[Value<'static>]) -> Vec<Value<'static>> {
    v.to_vec()
}

/// A view's columns, through the join every reflection tool runs.
#[test]
fn a_view_has_pg_attribute_rows() {
    let mut e = fixture();
    let r = rows(
        &mut e,
        "SELECT attname, attnum FROM pg_attribute \
          WHERE attrelid = 'vv'::regclass ORDER BY attnum",
    );
    assert_eq!(
        r.len(),
        1,
        "pg_attribute had no columns for the view: {r:?}"
    );
    assert_eq!(r[0][0], Value::text("id"));
    assert_eq!(r[0][1], Value::SmallInt(1));
    // information_schema knew this all along — the two must agree.
    let info = rows(
        &mut e,
        "SELECT column_name FROM information_schema.columns \
          WHERE table_name = 'vv' ORDER BY ordinal_position",
    );
    assert_eq!(info.len(), r.len());
}

/// A materialized view is 'm', not 'r'.
#[test]
fn a_materialized_view_reports_its_own_relkind() {
    let mut e = fixture();
    assert_eq!(
        one(&mut e, "SELECT relkind FROM pg_class WHERE relname = 'mv'"),
        Value::text("m"),
    );
    assert_eq!(
        one(&mut e, "SELECT relname FROM pg_class WHERE relkind = 'm'"),
        Value::text("mv"),
    );
    // A plain table is untouched by that.
    assert_eq!(
        one(&mut e, "SELECT relkind FROM pg_class WHERE relname = 't'"),
        Value::text("r"),
    );
}

/// relhasrules was hard-coded false with the note "SPG has no rule
/// system" — which stopped being true when CREATE RULE landed.
#[test]
fn relhasrules_answers_for_real() {
    let mut e = fixture();
    assert_eq!(
        one(
            &mut e,
            "SELECT relhasrules FROM pg_class WHERE relname = 't'"
        ),
        Value::Bool(false),
    );
    e.execute("CREATE RULE r_noop AS ON DELETE TO t DO INSTEAD NOTHING")
        .unwrap();
    assert_eq!(
        one(
            &mut e,
            "SELECT relhasrules FROM pg_class WHERE relname = 't'"
        ),
        Value::Bool(true),
    );
}
