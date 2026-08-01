//! Round 667 — `oid` becomes a type, not a spelling of `bigint`.
//!
//! `CREATE TABLE t(o OID)` answered `type "oid" does not exist` while the
//! neighbouring `t(x XID)` built fine, and `pg_typeof(1::oid)` said
//! `bigint`. The cast itself was already right — `4294967296::oid` and
//! `'abc'::oid` produce PG's errors word for word and `(-1)::oid` wraps to
//! 4294967295 — so only the resulting TYPE was being thrown away, by a
//! table that mapped the name straight to BigInt under a comment reading
//! "OIDs are plain integers".
//!
//! What this does NOT do, deliberately: a cell is still a `Value::BigInt`,
//! so `sum(oid)` and `avg(oid)` still answer where PG refuses. That is the
//! same limitation `DataType::Xid8` has documented since round 640, and
//! round 664 tried to close those two by name and withdrew, because a guard
//! keyed on the name catches `sum(bigint)` with it.
//!
//! The ledger entry that opened this said SPG "has no oid DataType" and
//! that `relfrozenxid` is declared bigint. Measured, the second half was
//! wrong: `xid` is a real type here and `pg_typeof(relfrozenxid)` has
//! answered `xid` all along. The tests below therefore also pin the
//! neighbours, because the way this change could go wrong is by disturbing
//! them.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
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
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).expect_err(sql))
}

/// PG18-verified: an OID column accepts a bare integer, and the value reads
/// back unchanged.
#[test]
fn round667_an_oid_column_takes_an_integer() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE o3(o OID)").unwrap();
    e.execute("INSERT INTO o3 VALUES (42::bigint)").unwrap();
    e.execute("INSERT INTO o3 VALUES (43::oid)").unwrap();
    e.execute("INSERT INTO o3 VALUES (44)").unwrap();
    assert_eq!(one(&mut e, "SELECT o FROM o3 ORDER BY o"), "42,43,44");
    assert_eq!(one(&mut e, "SELECT pg_typeof(o) FROM o3 LIMIT 1"), "oid");
}

/// The cast's range rules were already PG's; they are shared with the
/// column path now rather than copied, so they get pinned from both sides.
#[test]
fn round667_the_oid_domain_rules_hold_from_both_directions() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT 4294967295::oid"), "4294967295");
    // A negative wraps the way C's (Oid) cast does.
    assert_eq!(one(&mut e, "SELECT (-1)::oid"), "4294967295");
    assert!(err(&mut e, "SELECT 4294967296::oid").contains("OID out of range"));
    assert!(
        err(&mut e, "SELECT 'abc'::oid").contains("invalid input syntax for type oid"),
        "{}",
        err(&mut e, "SELECT 'abc'::oid")
    );

    // Same rules reaching a column, which is the path that used to have no
    // arm at all ("type mismatch in column ... expected OID, got INT").
    e.execute("CREATE TABLE o4(o OID)").unwrap();
    e.execute("INSERT INTO o4 VALUES (-1)").unwrap();
    assert_eq!(one(&mut e, "SELECT o FROM o4"), "4294967295");
    assert!(err(&mut e, "INSERT INTO o4 VALUES (4294967296)").contains("OID out of range"));
}

/// `pg_typeof` prefers the declared type for exactly three types — the ones
/// whose value carries no identity of its own. Round 640 wrote that list
/// with two entries and said "only those two"; this is the third, and the
/// point of the test is that it did not become a general switch.
#[test]
fn round667_pg_typeof_prefers_the_declared_type_for_oid_and_no_one_new() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT pg_typeof(1::oid)"), "oid");
    assert_eq!(one(&mut e, "SELECT pg_typeof(NULL::oid)"), "oid");
    // The two that were already there.
    assert_eq!(one(&mut e, "SELECT pg_typeof('7'::xid8)"), "xid8");
    assert_eq!(one(&mut e, "SELECT pg_typeof('7'::xid)"), "xid");
    // And the types whose value DOES know itself still answer from the
    // value — a general "prefer declared" switch would break these.
    assert_eq!(one(&mut e, "SELECT pg_typeof(1::int2)"), "smallint");
    assert_eq!(one(&mut e, "SELECT pg_typeof(1::float8)"), "double precision");
    assert_eq!(one(&mut e, "SELECT pg_typeof(1::numeric)"), "numeric");
    assert_eq!(one(&mut e, "SELECT pg_typeof(1::bigint)"), "bigint");
}

/// A regclass IS an oid. Giving `oid` its own DataType turned
/// `'text'::regtype::oid` from a coercion into BIGINT into one into OID,
/// and nine catalog tests went red at once — the reg* trio has to be named
/// in a THIRD list now.
#[test]
fn round667_the_reg_family_still_coerces_to_oid() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT 'text'::regtype::oid"), "25");
    assert_eq!(one(&mut e, "SELECT 'pg_class'::regclass::oid"), "1259");
    assert_eq!(one(&mut e, "SELECT pg_typeof('pg_class'::regclass)"), "regclass");
    // Into a declared OID column, which is the shape that broke.
    e.execute("CREATE TABLE o5(o OID)").unwrap();
    e.execute("INSERT INTO o5 SELECT 'text'::regtype::oid").unwrap();
    assert_eq!(one(&mut e, "SELECT o FROM o5"), "25");
}

/// The neighbour the ledger was wrong about. `xid` is a real type here and
/// always has been — the entry that opened this round claimed
/// `relfrozenxid` was declared bigint, and it is not.
///
/// `age(xid)` is deliberately NOT asserted here. Checking it turned up a
/// pre-existing inconsistency that this round did not cause and does not
/// fix: the documented "honestly 0" (SPG's u64 tx ids never wrap, so there
/// is no wraparound distance to report) is keyed on the value arriving as
/// an integer, and a real `Value::Xid` slips past it into the timestamp
/// path. Measured: on the wire `age('7'::xid)` is 0 but `age('0'::xid)` is
/// 2, and the embedded engine answers 1 for the first of those. Recorded
/// rather than pinned, because pinning any of those three numbers would be
/// pinning the bug.
#[test]
fn round667_xid_is_untouched() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(relfrozenxid) FROM pg_class LIMIT 1"),
        "xid"
    );
    e.execute("CREATE TABLE x1(x XID)").unwrap();
    e.execute("INSERT INTO x1 VALUES ('7'::xid)").unwrap();
    assert_eq!(one(&mut e, "SELECT pg_typeof(x) FROM x1"), "xid");
}
