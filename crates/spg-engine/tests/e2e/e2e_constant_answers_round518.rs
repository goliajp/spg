//! v7.39 (round 518) — functions that answered a constant.
//!
//! Round 517 found `pg_visible_in_snapshot` returning TRUE for everything,
//! under a comment reading "no MVCC-yet model" — a stub that outlived its
//! reason by several releases and that a caller could not tell from a real
//! answer. It was found by accident: adding a second spelling made the
//! compiler report an unreachable arm.
//!
//! `scripts/constant-answer-probe.py` looks for the rest mechanically. A
//! stub is invisible when you call it once and obvious when you call it
//! TWICE with inputs that should disagree, so every case is a pair, and a
//! function is suspect when SPG answers the same thing both times while PG
//! does not. Twelve came back. Nine are closed here.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT)").unwrap();
    e
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

/// The visibility probes answered TRUE for an oid that references NOTHING.
/// PG looks the object up first, so a tool asking about a dropped relation
/// was told it was visible.
#[test]
fn round518_visibility_probes_look_the_object_up() {
    let mut e = engine();
    assert_eq!(text(&mut e, "SELECT pg_table_is_visible('t'::regclass)"), "true");
    assert_eq!(text(&mut e, "SELECT pg_table_is_visible(999999::oid)"), "NULL");
    assert_eq!(text(&mut e, "SELECT pg_type_is_visible(999999::oid)"), "NULL");
    assert_eq!(text(&mut e, "SELECT pg_function_is_visible(999999::oid)"), "NULL");
    // A regclass VALUE exists only because the cast resolved it, so it is
    // visible whatever this engine's oid tables know about the number —
    // getting that wrong is what a first cut did.
    assert_eq!(text(&mut e, "SELECT pg_table_is_visible('t'::regclass)"), "true");
}

/// The encoding table, both directions. They differ in how they fail, which
/// is why both are pinned.
#[test]
fn round518_encoding_conversions_read_a_table() {
    let mut e = engine();
    assert_eq!(
        text(
            &mut e,
            "SELECT pg_encoding_to_char(0), pg_encoding_to_char(6), pg_encoding_to_char(8)"
        ),
        "SQL_ASCII|UTF8|LATIN1"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT pg_char_to_encoding('SQL_ASCII'), pg_char_to_encoding('UTF8'), \
             pg_char_to_encoding('LATIN1')"
        ),
        "0|6|8"
    );
    // An unknown NUMBER is empty; an unknown NAME is -1.
    assert_eq!(text(&mut e, "SELECT pg_encoding_to_char(99)"), "");
    assert_eq!(text(&mut e, "SELECT pg_char_to_encoding('nosuch')"), "-1");
}

/// A filenode for an oid that names nothing is NULL, not 0.
#[test]
fn round518_relation_filenode_needs_a_relation() {
    let mut e = engine();
    assert_eq!(text(&mut e, "SELECT pg_relation_filenode(999999::oid)"), "NULL");
    assert_ne!(text(&mut e, "SELECT pg_relation_filenode('t'::regclass)"), "NULL");
}

/// The snapshot a caller is about to be compared against, in PG's own
/// `xmin:xmax:xip_list` form — and it reads back through the function that
/// consumes it.
#[test]
fn round518_txid_current_snapshot_is_readable() {
    let mut e = engine();
    let snap = text(&mut e, "SELECT txid_current_snapshot()::text");
    let parts: Vec<&str> = snap.split(':').collect();
    assert_eq!(parts.len(), 3, "xmin:xmax:xip — got {snap}");
    assert!(parts[0].parse::<u64>().is_ok(), "xmin in {snap}");
    assert!(parts[1].parse::<u64>().is_ok(), "xmax in {snap}");
    // Round 517's reader accepts what round 518's writer produces.
    assert_eq!(
        text(
            &mut e,
            "SELECT txid_visible_in_snapshot(0::bigint, txid_current_snapshot())"
        ),
        "true"
    );
}

/// `age(xid)` is how far back a transaction id is. It used to fail with
/// "age() needs DATE or TIMESTAMP" — and the reason was not the missing
/// overload but a clock rewrite that injected a midnight argument into any
/// single-argument `age()` whose operand was not an integer LITERAL.
#[test]
fn round518_age_of_a_transaction_id() {
    let mut e = engine();
    let a = text(&mut e, "SELECT age('1'::xid)");
    assert!(a.parse::<i64>().is_ok_and(|n| n >= 0), "got {a}");
    // The temporal overloads are untouched, including the zero-argument
    // shape whose arity check a first cut of that guard read past — it
    // panicked the connection thread.
    assert!(e.execute("SELECT age()").is_err());
    assert_ne!(
        text(&mut e, "SELECT age(TIMESTAMP '2020-01-01')"),
        "",
        "age(timestamp) still answers"
    );
}
