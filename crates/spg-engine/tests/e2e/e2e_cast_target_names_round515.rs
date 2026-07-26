//! v7.39 (round 515) — a type name resolves the same way whatever the value.
//!
//! Round 509 made a cast target get checked even when the operand is NULL,
//! and gave that check its OWN list of names. The list was a duplicate of
//! what the value path accepts, and it drifted three times:
//!
//!   r512  added `Value::Cid` and never registered `cid`
//!   r514  found that, and that `'5'::xid` answered a BIGINT
//!   r515  found six more — `'english'::regconfig` resolved while
//!         `NULL::regconfig` did not
//!
//! The duplication was the defect. Each family now declares its names once
//! and both paths read that declaration, so this file's job is to hold the
//! property the drift kept breaking: for every type SPG knows, a NULL cast
//! and a value cast agree about whether the NAME exists.
//!
//! Every expectation is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    Engine::new()
}

fn ok_null(e: &mut Engine, ty: &str) -> bool {
    matches!(
        e.execute(&format!("SELECT NULL::{ty}")),
        Ok(QueryResult::Rows { .. })
    )
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .and_then(|r| r.values.first())
            .map(spg_engine::eval::value_to_text)
            .unwrap_or_default(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// Every type name the engine resolves for a VALUE also resolves for a
/// NULL. This is the property that broke three times.
#[test]
fn round515_a_null_cast_resolves_every_name_a_value_cast_does() {
    let mut e = engine();
    for ty in [
        // reg* — the six this round found
        "regproc",
        "regprocedure",
        "regoper",
        "regoperator",
        "regconfig",
        "regdictionary",
        "regcollation",
        "regnamespace",
        "regrole",
        "regclass",
        "regtype",
        // the catalog-shaped scalars
        "cid",
        "xid",
        "tid",
        "oidvector",
        "int2vector",
        "aclitem",
        "refcursor",
        "pg_snapshot",
        "txid_snapshot",
        "jsonpath",
        // the pseudotypes and internals
        "anyarray",
        "anyelement",
        "internal",
        "pg_ndistinct",
        "pg_mcv_list",
        "pg_dependencies",
        "gtsvector",
        "record",
        "cstring",
    ] {
        assert!(ok_null(&mut e, ty), "NULL::{ty} must resolve");
    }
    // And a name that is not a type still errors, whatever the operand —
    // the check this round did not weaken.
    assert!(!ok_null(&mut e, "nosuchtype"));
    assert!(e.execute("SELECT 1::nosuchtype").is_err());
}

/// `<element>[]` follows its element. A general rule, so the next scalar
/// added does not need a fourth entry somewhere.
#[test]
fn round515_an_array_of_a_known_type_is_a_known_type() {
    let mut e = engine();
    for ty in ["cstring[]", "aclitem[]", "\"char\"[]", "text[]", "int[]"] {
        assert!(ok_null(&mut e, ty), "NULL::{ty} must resolve");
    }
    // The element's own check runs over each member.
    assert_eq!(
        text(&mut e, "SELECT '{bench=arwdDxt/bench}'::aclitem[]::text"),
        "{bench=arwdDxt/bench}"
    );
    let err = format!("{}", e.execute("SELECT '{a,b}'::aclitem[]").unwrap_err());
    assert!(err.contains("unrecognized key word: \"a\""), "got {err}");
}

/// The value path is unchanged — the refactor moved declarations, not
/// behaviour.
#[test]
fn round515_the_value_paths_still_answer_what_they_did() {
    let mut e = engine();
    e.execute("CREATE TABLE t (a INT)").unwrap();
    assert_eq!(
        text(
            &mut e,
            "SELECT 'english'::regconfig::text || '|' || 't'::regclass::text || '|' || \
             '5'::cid::text || '|' || '$.a'::jsonpath::text"
        ),
        "english|t|5|$.\"a\""
    );
    // A pseudotype still refuses a value.
    assert!(
        format!("{}", e.execute("SELECT 'x'::internal").unwrap_err())
            .contains("cannot accept a value of type internal")
    );
}
