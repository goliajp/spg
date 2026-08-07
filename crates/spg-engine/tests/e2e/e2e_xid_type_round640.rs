//! v7.39 (round 640) — `xid` was a bigint wearing a name.
//!
//! `Value::Xid` has existed since round 512, so a `'5'::xid` literal
//! already knew what it was. The DataType did not exist at all, and every
//! place that reads a declared type rather than a value said `bigint`:
//! `pg_typeof(NULL::xid)`, `pg_class.relfrozenxid`, `CREATE TABLE t (a
//! xid)` — which answered `type "xid" does not exist` to legal SQL.
//!
//! Two catalog-integrity defects fell out of the same gap, both measured
//! against PG, which has neither:
//!
//!   * **120 orphan `pg_attribute` rows.** Round 623 described `ctid` /
//!     `xmin` / `cmin` / `xmax` / `cmax` and typed them 27 / 28 / 29,
//!     which no `pg_type` row carried. PG has exactly one orphan (a
//!     dropped column, atttypid 0).
//!   * **16 dangling `pg_type.typarray` pointers.** `bit` said its array
//!     type was 1561, `int4range` said 3905, and so on for every type
//!     rounds 635 and 638 added — none of which `pg_type` lists, because
//!     SPG has no such array types (`pg_typeof(NULL::int4range[])` is
//!     `unknown`). Naming a type that does not exist is the false claim
//!     here; 0 is what PG itself writes for a type with no array type,
//!     and the number comes back when the array type ships.
//!
//! `age(relfrozenxid)` — the call the `age(xid)` overload exists for —
//! answered "age() needs DATE or TIMESTAMP, got bigint". Two separate
//! reasons: the column was a bigint, and the clock rewrite decides the
//! overload from the argument's SYNTAX (an integer literal or an `::xid`
//! cast), so a column reference was rewritten into the temporal path.
//! This is why round 627's `age` guard had to be reverted. Both fixed;
//! the overload is now chosen from the value, which no spelling escapes.
//!
//! NOT closed, measured and left for the operator surface, which is its
//! own unit: PG gives `xid` equality and hashing but NO ordering
//! operator, so `min(xid)` / `max(xid)` / `count(DISTINCT xid)` /
//! `array_agg(xid ORDER BY xid)` / `xid <= xid` all error there and all
//! answer here. In the other direction PG accepts `xid = integer`
//! (`xideqint4`) and SPG refuses it. And `xid8` has a declared type but
//! no value of its own, so it cannot refuse a bigint the way `xid` does.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(err) => err.to_string(),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

#[test]
fn round640_xid_names_itself() {
    let mut e = Engine::new();
    assert_eq!(vals(&mut e, "SELECT pg_typeof(NULL::xid)"), vec!["xid"]);
    assert_eq!(vals(&mut e, "SELECT pg_typeof(NULL::xid8)"), vec!["xid8"]);
    assert_eq!(vals(&mut e, "SELECT pg_typeof('5'::xid)"), vec!["xid"]);
    // `'xid'::regtype` went through a different table than `NULL::xid`
    // and answered `type "xid" does not exist`.
    assert_eq!(vals(&mut e, "SELECT 'xid'::regtype::text"), vec!["xid"]);
    assert_eq!(vals(&mut e, "SELECT 'xid8'::regtype::text"), vec!["xid8"]);
    assert_eq!(vals(&mut e, "SELECT 'tid'::regtype::text"), vec!["tid"]);
    assert_eq!(vals(&mut e, "SELECT 'cid'::regtype::text"), vec!["cid"]);
}

#[test]
fn round640_pg_type_carries_the_row_header_types() {
    let mut e = Engine::new();
    // typlen / typcategory / typbyval read off PG18. `tid` is 6 bytes —
    // neither a register width nor a rung on the 1/2/4/8 alignment
    // ladder — so it is the one that is passed by reference.
    assert_eq!(
        vals(
            &mut e,
            "SELECT typname, oid, typlen, typcategory, typbyval, typalign FROM pg_type \
             WHERE typname IN ('tid','xid','cid','xid8') ORDER BY oid"
        ),
        vec![
            "tid|27|6|U|false|s",
            "xid|28|4|U|true|i",
            "cid|29|4|U|true|i",
            "xid8|5069|8|U|true|d",
        ]
    );
}

#[test]
fn round640_no_catalog_row_points_at_a_type_nothing_carries() {
    let mut e = Engine::new();
    // 120 before: the six system attributes on every relation.
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM pg_attribute a LEFT JOIN pg_type t ON t.oid = a.atttypid \
             WHERE t.oid IS NULL"
        ),
        vec!["0"]
    );
    // 16 before: bit, varbit, regtype, record, the six multiranges and
    // the six ranges, each naming an array type SPG does not have.
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM pg_type t LEFT JOIN pg_type a ON a.oid = t.typarray \
             WHERE t.typarray <> 0 AND a.oid IS NULL"
        ),
        vec!["0"]
    );
}

#[test]
fn round640_xid_is_a_column_type() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE xt (a xid, b xid8, c int)").unwrap();
    // How PG takes one: the literal is unknown-typed and the column's
    // input function reads it.
    e.execute("INSERT INTO xt VALUES ('11', '12', 3)").unwrap();
    assert_eq!(vals(&mut e, "SELECT a, b, c FROM xt"), vec!["11|12|3"]);
    assert_eq!(
        vals(&mut e, "SELECT pg_typeof(a), pg_typeof(b) FROM xt"),
        vec!["xid|xid8"]
    );
    // A bare integer is refused, as PG refuses it ("column "a" is of
    // type xid but expression is of type integer") — there is no
    // int-to-xid cast on either engine.
    assert!(err(&mut e, "INSERT INTO xt VALUES (13, 14, 3)").contains("XID"));
    // The declared type reaches the catalogs, both of which read `???`
    // and 0 while the DataType was missing.
    assert_eq!(
        vals(
            &mut e,
            "SELECT attname, atttypid, format_type(atttypid, atttypmod) FROM pg_attribute \
             WHERE attrelid = (SELECT oid FROM pg_class WHERE relname = 'xt') \
             AND attname IN ('a','b') ORDER BY attname"
        ),
        vec!["a|28|xid", "b|5069|xid8"]
    );
}

#[test]
fn round640_frozen_xid_is_an_xid_and_age_reads_it() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a int)").unwrap();
    assert_eq!(
        vals(
            &mut e,
            "SELECT pg_typeof(relfrozenxid) FROM pg_class LIMIT 1"
        ),
        vec!["xid"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT pg_typeof(datfrozenxid) FROM pg_database LIMIT 1"
        ),
        vec!["xid"]
    );
    // The wraparound query a monitoring tool actually runs. PG answers
    // `integer`; SPG answered "age() needs DATE or TIMESTAMP, got
    // bigint" — a capability wall, not a formatting difference.
    assert_eq!(
        vals(
            &mut e,
            "SELECT pg_typeof(age(relfrozenxid)) FROM pg_class LIMIT 1"
        ),
        vec!["integer"]
    );
    // …and through a spelling the syntax-driven overload check could
    // never have recognised.
    assert_eq!(
        vals(
            &mut e,
            "SELECT pg_typeof(age(c.relfrozenxid)) FROM pg_class c LIMIT 1"
        ),
        vec!["integer"]
    );
}
