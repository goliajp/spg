//! v7.39 (read01 utils/adt, round 24) — pgstatfuncs.c: the
//! pg_backend_pid() / pg_stat_activity identity join (host slot +
//! thread-local on the server; embedded pins the 1 fallback) and
//! pg_stat_database's full PG18 column set. Byte-locked vs PG18.

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

#[test]
fn backend_pid_embedded_fallback() {
    let mut e = Engine::new();
    // No host slot in embedded runs: pid 1, stable across calls.
    assert_eq!(
        row_of(&mut e, "SELECT pg_backend_pid(), pg_backend_pid() = pg_backend_pid()"),
        vec!["1", "true"]
    );
}

#[test]
fn stat_database_full_column_set() {
    let mut e = Engine::new();
    // The PG18 column set in PG's order — monitoring queries project
    // these by name; checksum_failures previously didn't exist.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT conflicts, temp_files, temp_bytes, deadlocks, checksum_failures, \
             checksum_last_failure, sessions, sessions_killed, \
             parallel_workers_launched, stats_reset \
             FROM pg_stat_database WHERE datname = current_database()"
        ),
        vec!["0", "0", "0", "0", "0", "NULL", "0", "0", "0", "NULL"]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT xact_commit >= 0, blks_read >= 0, session_time = 0.0 \
             FROM pg_stat_database WHERE datname = current_database()"
        ),
        vec!["true", "true", "true"]
    );
}
