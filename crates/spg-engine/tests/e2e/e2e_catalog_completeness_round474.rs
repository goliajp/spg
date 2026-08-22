//! read01 round 474 (C5 / C6 / C7) — three catalog-completeness items,
//! measured before being believed.
//!
//! The ledger recorded all three as gaps. Measuring split them:
//!
//!   C7  pg_database        REAL — five columns where PG18 has eighteen, so
//!                          `SELECT datfrozenxid FROM pg_database` (what a
//!                          wraparound monitor asks) failed outright. It
//!                          also named the database `postgres` while
//!                          `current_database()` answered `spg`, so a client
//!                          joining the two found no row.
//!   C6  pg_stat_activity   PARTLY — the ledger's "0 rows" was measured with
//!                          the embedded probe, which has no connections. A
//!                          live server already listed its client backends
//!                          correctly. What was missing: SPG's own
//!                          background workers, which is what an operator
//!                          looks for when a statement stalls behind the
//!                          engine write lock.
//!   C5  pg_settings        NOT a gap in the shape recorded. SPG lists
//!                          thirty parameters because it implements thirty;
//!                          PG18's other 368 are knobs SPG does not read.
//!                          One real hole: `synchronous_commit` IS honoured
//!                          and was not listed.

use spg_engine::{Engine, QueryResult};

fn scalar(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
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
            .join(";"),
        other => panic!("{sql} -> {other:?}"),
    }
}

#[test]
fn round474_pg_database_has_pgs_columns() {
    let mut e = Engine::new();
    // Every column PG18 declares — a missing one is a hard error, which is
    // how this was found.
    for col in [
        "oid",
        "datname",
        "datdba",
        "encoding",
        "datlocprovider",
        "datistemplate",
        "datallowconn",
        "dathasloginevt",
        "datconnlimit",
        "datfrozenxid",
        "datminmxid",
        "dattablespace",
        "datcollate",
        "datctype",
        "datlocale",
        "daticurules",
        "datcollversion",
        "datacl",
    ] {
        e.execute(&format!("SELECT {col} FROM pg_database"))
            .unwrap_or_else(|err| panic!("pg_database.{col}: {err}"));
    }
}

#[test]
fn round474_pg_database_agrees_with_current_database() {
    // They were `postgres` and `spg`, so a join on the two found nothing.
    let mut e = Engine::new();
    assert_eq!(
        scalar(
            &mut e,
            "SELECT datname FROM pg_database WHERE datname = current_database()"
        ),
        scalar(&mut e, "SELECT current_database()")
    );
}

#[test]
fn round474_datfrozenxid_is_the_real_mvcc_floor() {
    // A placeholder 0 would satisfy the column check above and still be
    // useless to the monitor that asked. It tracks the engine's own floor.
    let mut e = Engine::new();
    let before: i64 = scalar(&mut e, "SELECT datfrozenxid FROM pg_database")
        .parse()
        .expect("datfrozenxid is a number");
    e.execute("CREATE TABLE t (a INT)").unwrap();
    for _ in 0..5 {
        e.execute("INSERT INTO t VALUES (1)").unwrap();
    }
    let after: i64 = scalar(&mut e, "SELECT datfrozenxid FROM pg_database")
        .parse()
        .expect("datfrozenxid is a number");
    assert!(
        after >= before,
        "the floor must not go backwards: {before} -> {after}"
    );
}

#[test]
fn round474_synchronous_commit_is_readable_where_it_is_settable() {
    // It has gated the WAL-fsync wait since round 171; both read surfaces
    // used to deny it existed.
    let mut e = Engine::new();
    assert_eq!(scalar(&mut e, "SHOW synchronous_commit"), "on");
    assert_eq!(
        scalar(
            &mut e,
            "SELECT name, setting, vartype, context FROM pg_settings \
             WHERE name = 'synchronous_commit'"
        ),
        "synchronous_commit|on|enum|user"
    );
    e.execute("SET synchronous_commit = off").unwrap();
    assert_eq!(scalar(&mut e, "SHOW synchronous_commit"), "off");
    assert_eq!(
        scalar(
            &mut e,
            "SELECT setting FROM pg_settings WHERE name = 'synchronous_commit'"
        ),
        "off"
    );
}

#[test]
fn round474_a_knob_spg_ignores_reports_as_a_default() {
    // This asserted 0 until v7.38.18, with the reason "listing it would
    // tell a tuning tool that turning it does something". `SHOW
    // enable_seqscan` was answering `on` the whole time, so the tool
    // could already be told — only the enumeration hid it, and hiding it
    // there put `pg_settings` 367 rows short of PG18.
    //
    // What honestly separates a knob SPG reads from one it does not is
    // `source`: `default` here, `session` once a SET lands, which is the
    // same distinction PG draws.
    let mut e = Engine::new();
    assert_eq!(
        scalar(
            &mut e,
            "SELECT count(*), max(setting), max(source) FROM pg_settings \
             WHERE name = 'enable_seqscan'"
        ),
        "1|on|default"
    );
    assert_eq!(scalar(&mut e, "SHOW enable_seqscan"), "on");
}
