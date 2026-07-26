//! v7.39 (round 514) — the catalog-shaped scalar types.
//!
//! What was left of the function sweep's type inventory once the `reg*`
//! family closed: the ids, the oid vectors, an ACL item, a cursor name, a
//! transaction snapshot, a jsonpath, and the statistics / GiST internal
//! representations.
//!
//! Two were not new types at all but loose ends from earlier rounds. Round
//! 512 added `Value::Cid` for the `cmin` / `cmax` columns and never
//! registered `cid` as a cast target, so `NULL::cid` was an unknown type
//! while `NULL::xid` resolved. And `'5'::xid` answered a BIGINT — the name
//! resolved through the type table, not to the xid the system columns
//! carry — so `pg_typeof` disagreed with itself depending on where the
//! value came from.
//!
//! `jsonpath` already worked everywhere it mattered: `@?` and
//! `jsonb_path_query_first` take one today. Only the explicit cast was
//! missing, and PG normalises on input — `$.a` reads back `$."a"`.
//!
//! Every expectation below is a PG18 reading, including each error, which
//! differ per type and per ELEMENT: `::oidvector` complains about `oid`
//! where `::int2vector` complains about `smallint`.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    Engine::new()
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .unwrap_or_default(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).expect_err(sql))
}

/// The ids round-trip and report themselves, from either direction.
#[test]
fn round514_cid_and_xid_are_their_own_types() {
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT '5'::cid::text, pg_typeof('5'::cid), '5'::xid::text, pg_typeof('5'::xid)"
        ),
        "5|cid|5|xid"
    );
    assert!(err(&mut e, "SELECT 'x'::cid").contains("invalid input syntax for type cid: \"x\""));
    assert!(err(&mut e, "SELECT 'x'::xid").contains("invalid input syntax for type xid: \"x\""));
}

/// The oid vectors keep their spelling, and reject by ELEMENT — each with
/// the element type's own name.
#[test]
fn round514_the_oid_vectors_validate_element_by_element() {
    let mut e = engine();
    assert_eq!(
        text(&mut e, "SELECT '1 2 3'::oidvector::text, '1 2 3'::int2vector::text"),
        "1 2 3|1 2 3"
    );
    assert!(err(&mut e, "SELECT 'x'::oidvector").contains("invalid input syntax for type oid: \"x\""));
    assert!(
        err(&mut e, "SELECT '1 x'::int2vector")
            .contains("invalid input syntax for type smallint: \"x\"")
    );
}

/// An ACL item is `grantee=privileges/grantor`, and PG checks the key word
/// before anything else.
#[test]
fn round514_aclitem_keeps_its_shape() {
    let mut e = engine();
    assert_eq!(
        text(&mut e, "SELECT 'bench=arwdDxt/bench'::aclitem::text"),
        "bench=arwdDxt/bench"
    );
    assert!(err(&mut e, "SELECT 'x'::aclitem").contains("unrecognized key word: \"x\""));
}

/// A cursor name is a name; a snapshot is `xmin:xmax:xip_list`.
#[test]
fn round514_refcursor_and_snapshots() {
    let mut e = engine();
    assert_eq!(text(&mut e, "SELECT 'mycur'::refcursor::text"), "mycur");
    assert_eq!(
        text(
            &mut e,
            "SELECT '10:20:'::pg_snapshot::text, '10:20:15,17'::txid_snapshot::text"
        ),
        "10:20:|10:20:15,17"
    );
    assert!(
        err(&mut e, "SELECT 'x'::pg_snapshot")
            .contains("invalid input syntax for type pg_snapshot: \"x\"")
    );
}

/// PG normalises a jsonpath on input, so the cast is not a passthrough.
#[test]
fn round514_jsonpath_is_normalised_on_input() {
    let mut e = engine();
    assert_eq!(text(&mut e, "SELECT '$.a'::jsonpath::text"), "$.\"a\"");
    // And the operators that already took one keep working.
    assert_eq!(text(&mut e, "SELECT '{\"a\":1}'::jsonb @? '$.a'"), "true");
}

/// The statistics and GiST internals: a NULL is NULL, a value is refused
/// with PG's wording. That is the same contract the pseudotypes have, which
/// is why they share it.
#[test]
fn round514_statistics_internals_accept_null_and_refuse_values() {
    let mut e = engine();
    for ty in [
        "pg_ndistinct",
        "pg_mcv_list",
        "pg_dependencies",
        "gtsvector",
    ] {
        assert_eq!(text(&mut e, &format!("SELECT NULL::{ty}")), "NULL", "{ty}");
        let got = err(&mut e, &format!("SELECT 'x'::{ty}"));
        assert!(
            got.contains(&format!("cannot accept a value of type {ty}")),
            "{ty}: got {got}"
        );
    }
}
