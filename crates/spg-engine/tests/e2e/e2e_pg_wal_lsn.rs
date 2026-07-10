//! v7.37.17 (17.6 siblings) — pg_current_wal_lsn family returns
//! text "0/0" instead of NULL; pg_wal_lsn_diff parses hex/hex LSNs.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

fn bigint(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected BigInt, got {other:?}"),
    }
}

#[test]
fn pg_current_wal_lsn_family_returns_zero_text() {
    let mut e = Engine::new();
    for f in &[
        "pg_current_wal_lsn()",
        "pg_current_wal_flush_lsn()",
        "pg_current_wal_insert_lsn()",
        "pg_last_wal_receive_lsn()",
        "pg_last_wal_replay_lsn()",
    ] {
        let sql = format!("SELECT {f}");
        assert_eq!(text(&first(&mut e, &sql)), "0/0");
    }
}

#[test]
fn pg_wal_lsn_diff_basic() {
    let mut e = Engine::new();
    // 0/1000 = 4096 bytes; 0/0 = 0 bytes. diff = 4096.
    assert_eq!(
        bigint(&first(&mut e, "SELECT pg_wal_lsn_diff('0/1000', '0/0')")),
        4096
    );
    // Same LSN → 0.
    assert_eq!(
        bigint(&first(&mut e, "SELECT pg_wal_lsn_diff('0/0', '0/0')")),
        0
    );
    // 1/0 - 0/0 = 2^32.
    assert_eq!(
        bigint(&first(&mut e, "SELECT pg_wal_lsn_diff('1/0', '0/0')")),
        1i64 << 32
    );
    // Negative diff.
    assert_eq!(
        bigint(&first(&mut e, "SELECT pg_wal_lsn_diff('0/0', '0/1000')")),
        -4096
    );
}

#[test]
fn pg_wal_lsn_diff_bad_input_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT pg_wal_lsn_diff('bogus', '0/0')").is_err());
    assert!(e.execute("SELECT pg_wal_lsn_diff('zzz/0', '0/0')").is_err());
}

#[test]
fn pg_wal_lsn_diff_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_wal_lsn_diff(NULL::text, '0/0')"),
        spg_storage::Value::Null
    ));
}
