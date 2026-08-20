//! 7.38.7 — the three defects the sentori dogfood fixture found in its
//! first run, pinned as unit-level cases.
//!
//! The fixture itself (`xtests/dogfood_replay/fixtures/sentori-…`) is a
//! 21 MB dump, a `kill -9` and a reopen, which is the right shape for a
//! release gate and the wrong shape for a fast one. These are the same
//! failures in the smallest form that still reproduces them, so they run
//! in every `cargo test`.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn pin_v7387_scalar_subquery_materialises_a_uuid() {
    // The refusal that stopped the fixture: "subquery result type uuid
    // not yet materialisable". Every one of sentori's 27 primary keys is
    // a uuid, and the whitelist this went through had grown one reported
    // incident at a time. A type with a text input syntax needs no entry
    // — its own rendering, cast back to itself, reconstructs it.
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id UUID, ip INET, m MACADDR, d DATE, mo MONEY)")
        .unwrap();
    e.execute(
        "INSERT INTO u VALUES ('11111111-1111-4111-8111-111111111111', '10.0.0.1', \
         '08:00:2b:01:02:03', DATE '2026-01-02', 12.34::money)",
    )
    .unwrap();
    for (col, ty) in [
        ("id", "uuid"),
        ("ip", "inet"),
        ("m", "macaddr"),
        ("d", "date"),
        ("mo", "money"),
    ] {
        let sql = format!("SELECT count(*) FROM u WHERE {col} = (SELECT {col} FROM u LIMIT 1)");
        assert_eq!(one(&mut e, &sql), "1", "{ty} did not materialise");
    }
}

#[test]
fn pin_v7387_the_type_oid_table_has_no_zeroes_for_these() {
    // The cause underneath: `pg_type_oid` fell to `_ => 0` for the
    // network / money / time-of-day family, and that function is where
    // `pg_attribute.atttypid` comes from — so those columns had been
    // reporting type OID 0 to anything reflecting on the schema, and
    // `format_type` had no way back from 0 to a name.
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE oids (a INET, b CIDR, c MACADDR, d MONEY, e TIME, \
         f INTERVAL, g TSVECTOR)",
    )
    .unwrap();
    let zeroes = one(
        &mut e,
        "SELECT count(*) FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
         WHERE c.relname = 'oids' AND a.attnum > 0 AND a.atttypid = 0",
    );
    assert_eq!(
        zeroes, "0",
        "columns still report type OID 0 — pg_type_oid is missing entries, \
         and every reflection path reads that as 'no type at all'"
    );
}
