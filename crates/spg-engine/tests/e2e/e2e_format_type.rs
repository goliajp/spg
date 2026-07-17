//! v7.37.17 (17.6 siblings) — format_type upgraded from "unknown"
//! stub to real oid → SQL-standard name mapping with typmod
//! rendering.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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
    assert_eq!(
        text(&first(&mut e, "SELECT format_type(23, -1)")),
        "integer"
    );
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
    assert_eq!(
        text(&first(&mut e, "SELECT format_type('int4'::regtype, NULL)")),
        "integer"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT format_type('varchar'::regtype, 14)")),
        "character varying(10)"
    );
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT format_type('numeric'::regtype, 655366)"
        )),
        "numeric(10,2)"
    );
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT format_type('timestamp'::regtype, 3)"
        )),
        "timestamp(3) without time zone"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT format_type('bool'::regtype, NULL)")),
        "boolean"
    );
}

// read01 — a numeric OID cast to `::regtype` renders the type name (the
// common `atttypid::regtype` shape from pg_attribute introspection);
// an unrecognised OID renders the number, as PG does. vs live PG 18.4.
#[test]
fn regtype_numeric_oid_renders_type_name() {
    let mut e = Engine::new();
    assert_eq!(text(&first(&mut e, "SELECT 23::regtype::text")), "integer");
    assert_eq!(
        text(&first(&mut e, "SELECT 1043::regtype::text")),
        "character varying"
    );
    assert_eq!(text(&first(&mut e, "SELECT 20::regtype::text")), "bigint");
    assert_eq!(
        text(&first(&mut e, "SELECT 1114::regtype::text")),
        "timestamp without time zone"
    );
    assert_eq!(text(&first(&mut e, "SELECT 3802::regtype::text")), "jsonb");
    // Unknown OID falls back to the number.
    assert_eq!(text(&first(&mut e, "SELECT 99999::regtype::text")), "99999");
}

#[test]
fn format_type_renders_array_as_element_brackets() {
    // PG renders an array type as `<element>[]`, not the internal `_int4`
    // spelling. Live-PG18.4-verified.
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT format_type('_int4'::regtype, null)")),
        "integer[]"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT format_type('_text'::regtype, null)")),
        "text[]"
    );
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT format_type('_numeric'::regtype, null)"
        )),
        "numeric[]"
    );
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT format_type('_timestamp'::regtype, null)"
        )),
        "timestamp without time zone[]"
    );
    // Scalar spelling + typmod unchanged.
    assert_eq!(
        text(&first(&mut e, "SELECT format_type('int4'::regtype, null)")),
        "integer"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT format_type('varchar'::regtype, 14)")),
        "character varying(10)"
    );
}

// v7.39 (read01 utils/adt, format_type.c) — numeric array OIDs, the bit
// family, and PG's typmod-GIVEN-but--1 specials. All values
// differential-locked against PG18.
#[test]
fn array_oids_bit_family_and_typmod_given_specials() {
    let mut e = Engine::new();
    // Numeric array-type OIDs deconstruct to `<element>[]`.
    assert_eq!(
        text(&first(&mut e, "SELECT format_type(1007, NULL)")),
        "integer[]"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT format_type(1009, NULL)")),
        "text[]"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT format_type(1015, 20)")),
        "character varying(16)[]"
    );
    // bit family: typmod is the raw bit count (no varlena offset).
    assert_eq!(
        text(&first(&mut e, "SELECT format_type(1560, 4)")),
        "bit(4)"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT format_type(1560, NULL)")),
        "bit"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT format_type(1562, 8)")),
        "bit varying(8)"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT format_type(1562, NULL)")),
        "bit varying"
    );
    // typmod GIVEN as -1 is not the same as NULL: bpchar/-1 must not
    // re-parse as CHARACTER(1); bit/-1 is quoted (keyword).
    assert_eq!(
        text(&first(&mut e, "SELECT format_type(1042, -1)")),
        "bpchar"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT format_type(1042, NULL)")),
        "character"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT format_type(1560, -1)")),
        "\"bit\""
    );
    assert_eq!(
        text(&first(&mut e, "SELECT format_type(1562, -1)")),
        "bit varying"
    );
}
