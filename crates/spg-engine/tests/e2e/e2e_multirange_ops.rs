//! v7.39 (read01 utils/adt, round 17) — multirangetypes.c operator/
//! function surface + misc.c knives. Byte-locked vs PG18.

use spg_engine::{Engine, QueryResult};

fn row_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(spg_engine::eval::value_to_text)
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn col_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn multirange_containment_and_algebra() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT '{[1,3)}'::int4multirange @> 2, \
             '{[1,3),[5,8)}'::int4multirange @> int4range(5,7), \
             int4range(5,7) <@ '{[1,3),[5,8)}'::int4multirange, \
             '{[1,3)}'::int4multirange @> 4"
        ),
        vec!["true", "true", "true", "false"]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT '{[1,3)}'::int4multirange + '{[2,5)}'::int4multirange, \
             '{[1,5)}'::int4multirange - '{[2,3)}'::int4multirange, \
             '{[1,5)}'::int4multirange * '{[3,8)}'::int4multirange"
        ),
        vec!["{[1,5)}", "{[1,2),[3,5)}", "{[3,5)}"]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT '{[1,3)}'::int4multirange && '{[2,5)}'::int4multirange, \
             '{[1,3)}'::int4multirange && '{[4,5)}'::int4multirange"
        ),
        vec!["true", "false"]
    );
}

#[test]
fn multirange_accessors_and_unnest() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT lower('{[1,3),[5,8)}'::int4multirange), \
             upper('{[1,3),[5,8)}'::int4multirange), \
             isempty('{}'::int4multirange), \
             range_merge('{[1,3),[5,8)}'::int4multirange)"
        ),
        vec!["1", "8", "true", "[1,8)"]
    );
    assert_eq!(
        col_of(&mut e, "SELECT unnest('{[1,3),[5,8)}'::int4multirange)"),
        vec!["[1,3)", "[5,8)"]
    );
}

#[test]
fn misc_knives() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(&mut e, "SELECT pg_basetype('int4'::regtype)"),
        vec!["integer"]
    );
    let err = e.execute("SELECT parse_ident('a.b.')").unwrap_err();
    assert!(
        format!("{err}").contains("string is not a valid identifier: \"a.b.\""),
        "{err}"
    );
}

// v7.39 (read01 round 18) — numutils.c / oid.c / oracle_compat.c:
// bytea trims, chr() limit errors, OID's unsigned-32 semantics, and
// the oid/name literal prefixes. Byte-locked vs PG18.
#[test]
fn round18_numutils_oid_oracle_compat() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT btrim('\\x001122'::bytea, '\\x00'::bytea), \
             ltrim('\\x0011'::bytea, '\\x00'::bytea), \
             rtrim('\\x1100'::bytea, '\\x00'::bytea)"
        ),
        vec!["\\x1122", "\\x11", "\\x11"]
    );
    // OID wraps negatives like a C cast; overflow errors.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT (-1)::oid, 4294967295::oid, oid '16384', 'abc' = 'abc'"
        ),
        vec!["4294967295", "4294967295", "16384", "true"]
    );
    let err = |e: &mut Engine, sql: &str| -> String { format!("{}", e.execute(sql).unwrap_err()) };
    assert!(err(&mut e, "SELECT 4294967296::oid").contains("OID out of range"));
    assert!(
        err(&mut e, "SELECT 'abc'::oid").contains("invalid input syntax for type oid: \"abc\"")
    );
    assert!(err(&mut e, "SELECT chr(0)").contains("null character not permitted"));
    assert!(
        err(&mut e, "SELECT chr(1114112)")
            .contains("requested character too large for encoding: 1114112")
    );
}
