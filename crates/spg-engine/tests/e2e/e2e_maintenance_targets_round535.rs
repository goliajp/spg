//! v7.39 (round 535) — REINDEX / CLUSTER / VACUUM validate their target.
//!
//! Round 534 found the wire answering `SHOW` from its own inventory
//! before the statement reached the engine. Sweeping the rest of that
//! interception list found two statements answered there outright:
//!
//!     REINDEX TABLE nosuchtable   PG18  ERROR: relation … does not exist
//!                                 SPG   REINDEX
//!     CLUSTER nosuchtable         PG18  ERROR: relation … does not exist
//!                                 SPG   CLUSTER
//!
//! and, by the same reading, a third that reached the engine and did
//! nothing quietly:
//!
//!     VACUUM nosuchtable          PG18  ERROR      SPG  VACUUM
//!
//! A maintenance script that misspells a table name was told it
//! succeeded. REINDEX and CLUSTER were swallowed TWICE — the wire
//! answered with a tag, and the parser had thrown the name away before
//! that anyway.
//!
//! The audit records REINDEX / CLUSTER / ANALYZE as verified passing.
//! What was verified is that the statement is accepted, which is not the
//! same as validating what it was pointed at — the round 521 lesson in
//! another place. ANALYZE was, and is, correct.
//!
//! SPG has neither index bloat to rebuild nor a clustering order to
//! impose, so the WORK stays a no-op. Only the target check is PG's.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::Engine;

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ok (a INT)").unwrap();
    e.execute("CREATE INDEX ix ON ok (a)").unwrap();
    e
}

/// A relation that is not there is refused, by all three.
#[test]
fn round535_missing_relation_is_refused() {
    let mut e = engine();
    for sql in [
        "REINDEX INDEX nosuch",
        "REINDEX TABLE nosuch",
        "REINDEX INDEX CONCURRENTLY nosuch",
        "CLUSTER nosuch",
        "VACUUM nosuch",
        "VACUUM FULL nosuch",
    ] {
        let err = e.execute(sql).expect_err(sql);
        assert!(
            format!("{err}").contains("nosuch"),
            "{sql}: message was {err}"
        );
    }
}

/// A relation that IS there is accepted — including an index name,
/// which is a relation too and which a table-only lookup refused.
#[test]
fn round535_existing_relation_is_accepted() {
    let mut e = engine();
    for sql in [
        "REINDEX TABLE ok",
        "REINDEX INDEX ix",
        "CLUSTER ok",
        "CLUSTER ok USING ix",
        "VACUUM ok",
        "VACUUM ANALYZE ok",
    ] {
        e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
    }
}

/// A schema target is checked against schemas, not relations.
#[test]
fn round535_schema_target_is_checked_as_a_schema() {
    let mut e = engine();
    e.execute("REINDEX SCHEMA public").unwrap();
    let err = e
        .execute("REINDEX SCHEMA nosuchschema")
        .expect_err("no such schema");
    assert!(
        format!("{err}").contains("nosuchschema"),
        "message was {err}"
    );
}

/// The forms that name nothing stay accepted.
#[test]
fn round535_whole_database_forms_name_nothing() {
    let mut e = engine();
    for sql in [
        "REINDEX SYSTEM postgres",
        "CLUSTER",
        "CLUSTER VERBOSE",
        "VACUUM",
    ] {
        e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
    }
}

/// ANALYZE was already right, and stays that way.
#[test]
fn round535_analyze_unchanged() {
    let mut e = engine();
    e.execute("ANALYZE ok").unwrap();
    assert!(e.execute("ANALYZE nosuch").is_err());
}
