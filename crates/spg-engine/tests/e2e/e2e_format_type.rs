//! v7.37.17 (17.6 siblings) — format_type upgraded from "unknown"
//! stub to real oid → SQL-standard name mapping with typmod
//! rendering.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn base_type_names() {
    let mut e = Engine::new();
    // PG's SQL-standard deparse names, not internal typnames.
    assert_eq!(text(&first(&mut e, "SELECT format_type(23, -1)")), "integer");
    assert_eq!(text(&first(&mut e, "SELECT format_type(20, -1)")), "bigint");
    assert_eq!(text(&first(&mut e, "SELECT format_type(25, -1)")), "text");
    assert_eq!(
        text(&first(&mut e, "SELECT format_type(701, -1)")),
        "double precision"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT format_type(1043, -1)")),
        "character varying"
    );
}

#[test]
fn typmod_rendering() {
    let mut e = Engine::new();
    // varchar(255): typmod = 255 + 4.
    assert_eq!(
        text(&first(&mut e, "SELECT format_type(1043, 259)")),
        "character varying(255)"
    );
    // numeric(10,2): typmod = ((10 << 16) | 2) + 4.
    assert_eq!(
        text(&first(&mut e, "SELECT format_type(1700, 655366)")),
        "numeric(10,2)"
    );
    // timestamp(3): precision goes before the tz qualifier.
    assert_eq!(
        text(&first(&mut e, "SELECT format_type(1114, 3)")),
        "timestamp(3) without time zone"
    );
}

#[test]
fn unknown_oid_and_null() {
    let mut e = Engine::new();
    // PG returns "???" for an unrecognized oid.
    assert_eq!(
        text(&first(&mut e, "SELECT format_type(999999, -1)")),
        "???"
    );
    assert!(matches!(
        first(&mut e, "SELECT format_type(NULL::int, -1)"),
        spg_storage::Value::Null
    ));
}

// read01 — SPG's `::regtype` yields the type name (no OID space), so
// format_type is commonly called as `format_type('int4'::regtype, …)`
// with a TEXT first argument. It must resolve the name and render the
// typmod, same as the OID path. Values vs live PG 18.4.
#[test]
fn format_type_accepts_regtype_name() {
    let mut e = Engine::new();
    assert_eq!(text(&first(&mut e, "SELECT format_type('int4'::regtype, NULL)")), "integer");
    assert_eq!(
        text(&first(&mut e, "SELECT format_type('varchar'::regtype, 14)")),
        "character varying(10)"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT format_type('numeric'::regtype, 655366)")),
        "numeric(10,2)"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT format_type('timestamp'::regtype, 3)")),
        "timestamp(3) without time zone"
    );
    assert_eq!(text(&first(&mut e, "SELECT format_type('bool'::regtype, NULL)")), "boolean");
}
