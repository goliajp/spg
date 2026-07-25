//! read01 round 437 — a temporary table's visibility in catalog listings.
//!
//! Round 436 gave temporary tables a per-session storage prefix but never
//! taught the listings about it, so `SHOW TABLES`, `pg_class`, `pg_tables`
//! and `information_schema.tables` handed every client the raw
//! `__spg_temp_5__tmp` names of every OTHER session's temp tables. That was
//! a leak this project introduced, so it is the first thing fixed here.
//!
//! Measured on both oracles, which agree:
//!   * MariaDB 11 — its own session's temp table appears in SHOW TABLES and
//!     information_schema.tables; a second connection sees only `perm`.
//!   * PG 18 — its own appears in information_schema.tables (under schema
//!     `pg_temp_22`); a second connection sees only `perm437`.
//!
//! So: list the caller's own temporary tables under their logical name, and
//! nobody else's at all. `Catalog::listed_name` is the single rule, applied
//! at every synth.
//!
//! OIDs keep coming from a table's RAW catalog position, because that is the
//! order `relation_oid` replays — a foreign session's temp table still
//! consumes its slot and is only left out of the output. The `regclass`
//! round-trips below are what pin that.

use spg_engine::{Engine, QueryResult};

fn cells(e: &mut Engine, sql: &str) -> String {
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

fn engine_with_temp() -> Engine {
    let mut e = Engine::new();
    e.set_current_session(1);
    e.execute("CREATE TABLE perm(i INT)").unwrap();
    e.execute("CREATE TEMPORARY TABLE mytmp(a INT)").unwrap();
    e
}

#[test]
fn round437_own_session_sees_its_temp_under_the_logical_name() {
    let mut e = engine_with_temp();
    for sql in [
        "SELECT table_name FROM information_schema.tables \
         WHERE table_name IN ('perm','mytmp') ORDER BY table_name",
        "SELECT relname FROM pg_class WHERE relname IN ('perm','mytmp') ORDER BY relname",
        "SELECT tablename FROM pg_tables WHERE tablename IN ('perm','mytmp') ORDER BY tablename",
    ] {
        assert_eq!(cells(&mut e, sql), "mytmp,perm", "{sql}");
    }
    // SHOW TABLES lists it too (order is catalog order, which neither
    // oracle guarantees — MariaDB's own answer is not sorted either).
    let shown = cells(&mut e, "SHOW TABLES");
    assert!(shown.contains("mytmp"), "{shown}");
    assert!(shown.contains("perm"), "{shown}");
}

#[test]
fn round437_another_session_sees_neither_the_temp_nor_its_mangled_name() {
    let mut e = engine_with_temp();
    e.set_current_session(2);
    assert_eq!(cells(&mut e, "SHOW TABLES"), "perm");
    assert_eq!(
        cells(
            &mut e,
            "SELECT COUNT(*) FROM information_schema.tables WHERE table_name LIKE '%tmp%'"
        ),
        "0"
    );
    // The storage name must never reach a client.
    for view in ["pg_class", "pg_tables"] {
        let col = if view == "pg_class" {
            "relname"
        } else {
            "tablename"
        };
        assert_eq!(
            cells(
                &mut e,
                &format!("SELECT COUNT(*) FROM {view} WHERE {col} LIKE '__spg_temp%'")
            ),
            "0",
            "{view} leaked a storage name"
        );
    }
}

#[test]
fn round437_oids_still_round_trip_for_both_kinds() {
    let mut e = engine_with_temp();
    assert_eq!(cells(&mut e, "SELECT 'mytmp'::regclass::text"), "mytmp");
    assert_eq!(
        cells(
            &mut e,
            "SELECT relname FROM pg_class WHERE oid = 'mytmp'::regclass"
        ),
        "mytmp"
    );
    assert_eq!(
        cells(
            &mut e,
            "SELECT relname FROM pg_class WHERE oid = 'perm'::regclass"
        ),
        "perm"
    );
}

#[test]
fn round437_a_session_with_no_temp_tables_lists_normally() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE a(i INT)").unwrap();
    e.execute("CREATE TABLE b(i INT)").unwrap();
    assert_eq!(
        cells(
            &mut e,
            "SELECT table_name FROM information_schema.tables \
             WHERE table_name IN ('a','b') ORDER BY table_name"
        ),
        "a,b"
    );
}
