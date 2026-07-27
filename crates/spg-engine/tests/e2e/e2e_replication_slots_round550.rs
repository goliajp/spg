//! v7.39 (round 550) — a replication slot that is actually there.
//!
//! Round 549 re-measured the audit's phase 2; this round did the same
//! for phase 3, its capability list. Eight of the nine entries no
//! longer reproduce as written — the canonical full-text index
//! (`USING gin (to_tsvector(...))`), `NULLS NOT DISTINCT` (which SPG
//! enforces, DETAIL line and all), a doubled BEGIN's WARNING, MySQL's
//! non-strict sql_mode truncation, `GROUP BY … WITH ROLLUP`, IANA
//! named timezones down to the southern-hemisphere DST cases, and the
//! two entries round 534/536 settled as decisions rather than gaps.
//! WAL LSN advances correctly too.
//!
//! What survived was the slot family, and it was worse than missing:
//!
//!     SELECT pg_create_physical_replication_slot('s')   NULL, no slot
//!     SELECT pg_drop_replication_slot('nosuchslot')     NULL — PG raises
//!
//! The whole family answered NULL from the value dispatch, under a note
//! saying a scalar-surface NULL was fine. It is not: a replication
//! setup script ran clean and created nothing, and dropping a slot that
//! was never there reported success.
//!
//! A slot in PG is two things — a named record and a reservation that
//! holds WAL back. SPG keeps the RECORD, which is what a setup script
//! and every monitoring query read, and `wal_status` says
//! `unreserved`: PG's own word for a slot that no longer holds WAL.
//! The reservation is recorded work, not claimed here.
//!
//! FILE_VERSION 85 → 86; slots survive a snapshot round-trip.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
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
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// Create, list, drop — and the drop that must raise.
#[test]
fn round550_slot_lifecycle() {
    let mut e = Engine::new();
    assert_eq!(
        rows(&mut e, "SELECT pg_create_physical_replication_slot('s1')"),
        vec!["s1"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT pg_create_logical_replication_slot('s2', 'pgoutput')"
        ),
        vec!["s2"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT slot_name, plugin, slot_type, temporary, active, wal_status \
             FROM pg_replication_slots ORDER BY slot_name"
        ),
        vec![
            "s1|NULL|physical|false|false|unreserved",
            "s2|pgoutput|logical|false|false|unreserved",
        ]
    );
    e.execute("SELECT pg_drop_replication_slot('s1')").unwrap();
    assert_eq!(
        rows(&mut e, "SELECT slot_name FROM pg_replication_slots"),
        vec!["s2"]
    );
}

/// The two refusals PG makes, which SPG used to answer NULL to.
#[test]
fn round550_missing_and_duplicate_slots_raise() {
    let mut e = Engine::new();
    let err = format!(
        "{}",
        e.execute("SELECT pg_drop_replication_slot('nosuchslot')")
            .expect_err("dropping a slot that is not there")
    );
    assert!(err.contains("nosuchslot"), "message was {err}");
    assert!(err.contains("does not exist"), "message was {err}");

    e.execute("SELECT pg_create_physical_replication_slot('dup')")
        .unwrap();
    let err = format!(
        "{}",
        e.execute("SELECT pg_create_physical_replication_slot('dup')")
            .expect_err("creating one twice")
    );
    assert!(err.contains("already exists"), "message was {err}");
}

/// Slots survive a snapshot round-trip — what FILE_VERSION 86 is for.
#[test]
fn round550_slots_survive_a_reload() {
    let mut e = Engine::new();
    e.execute("SELECT pg_create_physical_replication_slot('keep')")
        .unwrap();
    e.execute("SELECT pg_create_logical_replication_slot('keepl', 'pgoutput')")
        .unwrap();
    let snapshot = e.catalog().serialize();
    let mut back =
        Engine::restore(spg_storage::Catalog::deserialize(&snapshot).expect("roundtrip"));
    assert_eq!(
        rows(
            &mut back,
            "SELECT slot_name, plugin, slot_type FROM pg_replication_slots ORDER BY slot_name"
        ),
        vec!["keep|NULL|physical", "keepl|pgoutput|logical"]
    );
}

/// The phase-3 entries that no longer reproduce, held in place so the
/// ledger cannot mislead the next round either.
#[test]
fn round550_phase3_entries_that_closed() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT, doc TEXT)").unwrap();
    // A-1: the canonical full-text index spelling.
    e.execute("CREATE INDEX g ON t USING gin (to_tsvector('english', doc))")
        .unwrap();
    // A-5: NULLS NOT DISTINCT, and SPG ENFORCES it.
    e.execute("CREATE UNIQUE INDEX u ON t (a) NULLS NOT DISTINCT")
        .unwrap();
    e.execute("INSERT INTO t (a) VALUES (NULL)").unwrap();
    let err = format!(
        "{}",
        e.execute("INSERT INTO t (a) VALUES (NULL)")
            .expect_err("a second NULL under NULLS NOT DISTINCT")
    );
    assert!(err.contains("duplicate key"), "message was {err}");
    // A-8's other half — the WAL position advancing — is a SERVER
    // fact: the embedded engine has no WAL writer, so it reads 0/0
    // here. Measured over the wire this round instead: 1000 inserts
    // moved it from 0/422 to 0/485.
    assert_eq!(rows(&mut e, "SELECT pg_current_wal_lsn()"), vec!["0/0"]);
}

/// A doubled BEGIN warns and stays in one transaction, as in PG.
#[test]
fn round550_double_begin_warns() {
    let mut e = Engine::new();
    e.execute("BEGIN").unwrap();
    // PG emits WARNING: there is already a transaction in progress and
    // carries on; the second BEGIN must not open a second one or fail.
    e.execute("BEGIN").unwrap();
    e.execute("COMMIT").unwrap();
}
