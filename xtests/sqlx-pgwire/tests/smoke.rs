//! sqlx 0.8 against spg-server PG-wire — smoke suite for v7.9
//! features. All tests `#[ignore]` because they need a live
//! spg-server on $SPG_PG_URL. Run with:
//!
//!   docker run -d -p 5433:5432 -e SPG_PG_ADDR=0.0.0.0:5432 goliakk/spg:7.9.0
//!   export SPG_PG_URL='postgres://bench:bench@127.0.0.1:5433/bench'
//!   cargo test -p spg-sqlx-pgwire -- --ignored

use sqlx::postgres::PgPoolOptions;
use sqlx::{Row, postgres::PgPool};
use std::time::Duration;

/// v7.37 (round 1005) — `None` when there is no server to talk to.
///
/// Every test here is `#[ignore]`d because it needs a live spg-server on
/// `$SPG_PG_URL`, which the module docs say plainly. That was enough until
/// `gate.sh --full` began passing `--include-ignored`: all eleven then ran
/// and panicked with `SPG_PG_URL not set`, reporting a configuration
/// statement as ten failures.
///
/// The perf gate had already settled how this repo answers that — name
/// what is missing, skip, and let the release run be the thing that
/// insists. Returning `None` lets each test do the same in one line.
async fn pool() -> Option<PgPool> {
    let url = match std::env::var("SPG_PG_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!("skipping: SPG_PG_URL is unset, so there is no server to smoke-test");
            return None;
        }
    };
    Some(
        PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(5))
            .connect(&url)
            .await
            .expect("connect to spg-server PG-wire"),
    )
}

#[tokio::test]
#[ignore]
async fn jsonb_round_trip_via_serde_json() {
    let Some(pool) = pool().await else {
        return;
    };
    sqlx::query("DROP TABLE IF EXISTS sqlx_jsonb")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE sqlx_jsonb (id INT NOT NULL, payload JSONB)")
        .execute(&pool)
        .await
        .unwrap();
    let payload = serde_json::json!({"event": "delivered", "n": 42});
    sqlx::query("INSERT INTO sqlx_jsonb VALUES ($1, $2)")
        .bind(1_i32)
        .bind(&payload)
        .execute(&pool)
        .await
        .unwrap();
    let row = sqlx::query("SELECT payload FROM sqlx_jsonb WHERE id = $1")
        .bind(1_i32)
        .fetch_one(&pool)
        .await
        .unwrap();
    let got: serde_json::Value = row.try_get("payload").unwrap();
    assert_eq!(got, payload);
}

#[tokio::test]
#[ignore]
async fn timestamptz_decodes_into_datetime_utc() {
    use chrono::{DateTime, Utc};
    let Some(pool) = pool().await else {
        return;
    };
    sqlx::query("DROP TABLE IF EXISTS sqlx_ts")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE sqlx_ts (id INT NOT NULL, sent_at TIMESTAMPTZ NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sqlx_ts VALUES (1, '2026-06-04 12:00:00')")
        .execute(&pool)
        .await
        .unwrap();
    let row = sqlx::query("SELECT sent_at FROM sqlx_ts WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let _ts: DateTime<Utc> = row.try_get("sent_at").unwrap();
}

#[tokio::test]
#[ignore]
async fn returning_id_from_insert() {
    let Some(pool) = pool().await else {
        return;
    };
    sqlx::query("DROP TABLE IF EXISTS sqlx_ret")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE sqlx_ret (id BIGSERIAL, name TEXT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    let row = sqlx::query("INSERT INTO sqlx_ret (name) VALUES ('alice') RETURNING id")
        .fetch_one(&pool)
        .await
        .unwrap();
    let id: i64 = row.try_get("id").unwrap();
    assert!(id > 0);
}

#[tokio::test]
#[ignore]
async fn on_conflict_do_nothing_dedup() {
    let Some(pool) = pool().await else {
        return;
    };
    sqlx::query("DROP TABLE IF EXISTS sqlx_uniq")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE sqlx_uniq (id INT NOT NULL, v INT)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE INDEX sqlx_uniq_pk ON sqlx_uniq (id)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sqlx_uniq VALUES (1, 100)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sqlx_uniq VALUES (1, 999) ON CONFLICT (id) DO NOTHING")
        .execute(&pool)
        .await
        .unwrap();
    let row = sqlx::query("SELECT v FROM sqlx_uniq WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let v: i32 = row.try_get("v").unwrap();
    assert_eq!(v, 100);
}

#[tokio::test]
#[ignore]
async fn on_conflict_do_update_excluded() {
    let Some(pool) = pool().await else {
        return;
    };
    sqlx::query("DROP TABLE IF EXISTS sqlx_acc")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE sqlx_acc (id INT NOT NULL, hash TEXT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE INDEX sqlx_acc_pk ON sqlx_acc (id)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sqlx_acc VALUES (1, 'old')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO sqlx_acc VALUES (1, 'new') \
                 ON CONFLICT (id) DO UPDATE SET hash = EXCLUDED.hash",
    )
    .execute(&pool)
    .await
    .unwrap();
    let row = sqlx::query("SELECT hash FROM sqlx_acc WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let h: String = row.try_get("hash").unwrap();
    assert_eq!(h, "new");
}

#[tokio::test]
#[ignore]
async fn on_conflict_composite_target_do_update() {
    let Some(pool) = pool().await else {
        return;
    };
    sqlx::query("DROP TABLE IF EXISTS sqlx_cal")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE sqlx_cal (uid INT NOT NULL, cal_id INT NOT NULL, payload TEXT NOT NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO sqlx_cal VALUES (1, 100, 'v1')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO sqlx_cal VALUES (1, 100, 'v2') \
                 ON CONFLICT (uid, cal_id) DO UPDATE SET payload = EXCLUDED.payload",
    )
    .execute(&pool)
    .await
    .unwrap();
    let row = sqlx::query("SELECT payload FROM sqlx_cal WHERE uid = 1 AND cal_id = 100")
        .fetch_one(&pool)
        .await
        .unwrap();
    let p: String = row.try_get("payload").unwrap();
    assert_eq!(p, "v2");
}

#[tokio::test]
#[ignore]
async fn round27_returning_arithmetic_types_as_int_not_text() {
    // mailrs round-27 (P0): `RETURNING uidnext - 1 AS uid` was
    // wire-typed TEXT; the typed i32 decode rejected every delivery
    // index write. Server-path twin of
    // crates/spg-sqlx/tests/e2e/mailrs_round27.rs.
    let Some(pool) = pool().await else {
        return;
    };
    sqlx::query("DROP TABLE IF EXISTS r27_mb")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE r27_mb (id BIGSERIAL PRIMARY KEY, name TEXT, \
         uidnext INTEGER NOT NULL DEFAULT 1, hm BIGINT NOT NULL DEFAULT 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO r27_mb (name) VALUES ('INBOX')")
        .execute(&pool)
        .await
        .unwrap();
    let (id, uid, new_modseq): (i64, i32, i64) = sqlx::query_as(
        "UPDATE r27_mb SET uidnext = uidnext + 1, hm = hm + 1 WHERE name = $1 \
         RETURNING id, uidnext - 1 AS uid, hm AS new_modseq",
    )
    .bind("INBOX")
    .fetch_one(&pool)
    .await
    .expect("typed decode of arithmetic RETURNING over pgwire");
    assert_eq!((id, uid, new_modseq), (1, 1, 1));
}

/// v7.39 (round 443) — a transaction driven the way sqlx drives one must be
/// a transaction.
///
/// pgwire's `Execute` called `execute_prepared_with_cancel`, which hardcodes
/// `IMPLICIT_TX`, so a prepared `BEGIN` registered on slot 0 instead of the
/// connection's. ReadyForQuery then kept reporting 'I', every following DML
/// took the `tx_state == b'I'` group-commit route, and COMMIT closed a slot
/// holding none of the writes. Measured before the fix: 0 rows here, while
/// the server's WAL had recorded BEGIN / … / COMMIT — so a restart replayed
/// writes the live engine had never applied.
///
/// This lives here rather than in the raw-socket e2e because THIS is the
/// shape that reproduces it: statements prepared and executed through a pool,
/// against a server with a WAL (no WAL, no group-commit route, no defect).
/// Every driver that prepares — sqlx, JDBC, psycopg3 — is on this path; the
/// simple-query path was always correct, which is why psql never showed it.
#[tokio::test]
#[ignore]
async fn prepared_begin_commit_is_a_real_transaction() {
    let Some(pool) = pool().await else {
        return;
    };
    sqlx::query("DROP TABLE IF EXISTS sqlx_tx")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE sqlx_tx (id INT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();

    sqlx::query("BEGIN").execute(&pool).await.unwrap();
    for k in 0..3_i32 {
        sqlx::query("INSERT INTO sqlx_tx VALUES ($1)")
            .bind(k)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("COMMIT").execute(&pool).await.unwrap();

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlx_tx")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 3, "committed rows must be visible after COMMIT");
}

/// The other half, through sqlx's own transaction API — the idiomatic shape,
/// where the driver holds ONE connection for the whole transaction.
///
/// A raw `BEGIN` sent through a pool is not a transaction on either engine
/// (the pool is free to hand each statement a different connection, and PG18
/// keeps the rows too — measured). `pool.begin()` is, so this is where the
/// rollback contract can actually be asserted.
#[tokio::test]
#[ignore]
async fn transaction_api_rollback_discards_its_writes() {
    let Some(pool) = pool().await else {
        return;
    };
    sqlx::query("DROP TABLE IF EXISTS sqlx_tx_rb")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE sqlx_tx_rb (id INT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO sqlx_tx_rb VALUES (1)")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlx_tx_rb")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0, "a rolled-back transaction must leave no rows");
}

/// …and its commit counterpart on the same API.
#[tokio::test]
#[ignore]
async fn transaction_api_commit_persists_its_writes() {
    let Some(pool) = pool().await else {
        return;
    };
    sqlx::query("DROP TABLE IF EXISTS sqlx_tx_c")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE sqlx_tx_c (id INT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    for k in 0..3_i32 {
        sqlx::query("INSERT INTO sqlx_tx_c VALUES ($1)")
            .bind(k)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlx_tx_c")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 3, "a committed transaction must keep its rows");
}
