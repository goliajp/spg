//! mailrs embed round-12 — regression coverage for the gaps the
//! spg-embedded cutover spike surfaced (2026-06-10). Each test mirrors
//! a real mailrs query shape that failed against v7.20.0.

use spg_sqlx::{SpgPool, SpgPoolExt};
use sqlx::Row;

async fn pool() -> SpgPool {
    SpgPool::connect_in_memory().await.expect("in-memory spg")
}

/// Gap 1 — `Option<T>` binds (sqlx-core's Encode-for-Option is a
/// per-driver macro; without it every Option bind resolves to the
/// Postgres driver).
#[tokio::test]
async fn option_binds_encode_null_and_value() {
    let p = pool().await;
    sqlx::query("CREATE TABLE t1 (id BIGINT, note TEXT)")
        .execute(&p)
        .await
        .unwrap();
    sqlx::query("INSERT INTO t1 VALUES ($1, $2)")
        .bind(1_i64)
        .bind(Option::<&str>::None)
        .execute(&p)
        .await
        .unwrap();
    sqlx::query("INSERT INTO t1 VALUES ($1, $2)")
        .bind(2_i64)
        .bind(Some("hello"))
        .execute(&p)
        .await
        .unwrap();
    let rows: Vec<(i64, Option<String>)> = sqlx::query_as("SELECT id, note FROM t1 ORDER BY id")
        .fetch_all(&p)
        .await
        .unwrap();
    assert_eq!(rows, vec![(1, None), (2, Some("hello".into()))]);
}

/// Gap 2 — borrowed-slice binds (`= ANY($1)` with `&[i64]`).
#[tokio::test]
async fn slice_binds_any_array() {
    let p = pool().await;
    sqlx::query("CREATE TABLE t2 (id BIGINT)")
        .execute(&p)
        .await
        .unwrap();
    for i in 1..=4_i64 {
        sqlx::query("INSERT INTO t2 VALUES ($1)")
            .bind(i)
            .execute(&p)
            .await
            .unwrap();
    }
    let ids: &[i64] = &[2, 4];
    let rows: Vec<(i64,)> = sqlx::query_as("SELECT id FROM t2 WHERE id = ANY($1) ORDER BY id")
        .bind(ids)
        .fetch_all(&p)
        .await
        .unwrap();
    assert_eq!(rows, vec![(2,), (4,)]);
}

/// Gap 3 — `sqlx::raw_sql` multi-statement scripts (schema bootstrap
/// feeds whole .sql files through one call; PG splits server-side).
#[tokio::test]
async fn raw_sql_runs_multi_statement_scripts() {
    let p = pool().await;
    sqlx::raw_sql(
        "-- comment with ; inside\n\
         CREATE TABLE t3 (id BIGINT, label TEXT DEFAULT 'a;b');\n\
         CREATE INDEX idx_t3 ON t3(id);\n\
         INSERT INTO t3 (id) VALUES (1);\n\
         INSERT INTO t3 (id) VALUES (2);",
    )
    .execute(&p)
    .await
    .unwrap();
    let n: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM t3")
        .fetch_one(&p)
        .await
        .unwrap();
    assert_eq!(n.0, 2);
}

/// Gap 3 polish (v7.21) — a failing multi-statement script rolls
/// back as a unit. PG wraps every simple-query message in an
/// implicit transaction; a script that dies at statement N must not
/// leave statements 1..N-1 applied (half a schema is worse than no
/// schema for a bootstrap path).
#[tokio::test]
async fn raw_sql_script_rolls_back_atomically() {
    let p = pool().await;
    let r = sqlx::raw_sql(
        "CREATE TABLE t3b (id BIGINT);\n\
         INSERT INTO t3b VALUES (1);\n\
         INSERT INTO no_such_table VALUES (1);",
    )
    .execute(&p)
    .await;
    assert!(r.is_err(), "script must fail at statement #3");
    let probe = sqlx::query("SELECT COUNT(*) FROM t3b").fetch_one(&p).await;
    assert!(
        probe.is_err(),
        "t3b survived the rollback — the script applied partially"
    );
}

/// Gap 4 — `INSERT … RETURNING id` must carry the schema column's
/// type (BIGINT), not default to TEXT; sqlx type-checks the decode.
#[tokio::test]
async fn returning_carries_column_type() {
    let p = pool().await;
    sqlx::query("CREATE TABLE t4 (id BIGSERIAL PRIMARY KEY, name TEXT)")
        .execute(&p)
        .await
        .unwrap();
    let row: (i64,) = sqlx::query_as("INSERT INTO t4 (name) VALUES ($1) RETURNING id")
        .bind("x")
        .fetch_one(&p)
        .await
        .unwrap();
    assert_eq!(row.0, 1);
    // By-name access too (PG names the output after the column).
    let row = sqlx::query("INSERT INTO t4 (name) VALUES ($1) RETURNING id")
        .bind("y")
        .fetch_one(&p)
        .await
        .unwrap();
    let id: i64 = row.try_get("id").unwrap();
    assert_eq!(id, 2);
}

/// Gap 5 — `pg_extension` catalog probe (bare and qualified).
#[tokio::test]
async fn pg_extension_lists_native_capabilities() {
    let p = pool().await;
    let bare: (bool,) =
        sqlx::query_as("SELECT EXISTS(SELECT 1 FROM pg_extension WHERE extname = 'vector')")
            .fetch_one(&p)
            .await
            .unwrap();
    assert!(bare.0);
    let qualified: (bool,) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM pg_catalog.pg_extension WHERE extname = 'vector')",
    )
    .fetch_one(&p)
    .await
    .unwrap();
    assert!(qualified.0);
}

/// Gap 6 — bitwise operators `|`, `&`, `~` (mailrs IMAP flag masks).
#[tokio::test]
async fn bitwise_operators_on_integers() {
    let p = pool().await;
    sqlx::query("CREATE TABLE t6 (id BIGINT, flags INTEGER NOT NULL DEFAULT 0)")
        .execute(&p)
        .await
        .unwrap();
    sqlx::query("INSERT INTO t6 VALUES (1, 5)")
        .execute(&p)
        .await
        .unwrap();
    sqlx::query("UPDATE t6 SET flags = flags | $1 WHERE id = 1")
        .bind(2_i32)
        .execute(&p)
        .await
        .unwrap();
    let v: (i32,) = sqlx::query_as("SELECT flags FROM t6 WHERE id = 1")
        .fetch_one(&p)
        .await
        .unwrap();
    assert_eq!(v.0, 7);
    sqlx::query("UPDATE t6 SET flags = flags & ~$1 WHERE id = 1")
        .bind(4_i32)
        .execute(&p)
        .await
        .unwrap();
    let v: (i32,) = sqlx::query_as("SELECT flags FROM t6 WHERE id = 1")
        .fetch_one(&p)
        .await
        .unwrap();
    assert_eq!(v.0, 3);
    let filtered: Vec<(i64,)> = sqlx::query_as("SELECT id FROM t6 WHERE (flags & $1) != 0")
        .bind(1_i32)
        .fetch_all(&p)
        .await
        .unwrap();
    assert_eq!(filtered.len(), 1);
}

/// Gap 7 — `ON CONFLICT (col) DO UPDATE` against a standalone
/// `CREATE UNIQUE INDEX` (not an inline UNIQUE constraint). mailrs's
/// suppression-list upsert uses exactly this shape.
#[tokio::test]
async fn on_conflict_upsert_via_unique_index() {
    let p = pool().await;
    sqlx::raw_sql(
        "CREATE TABLE t7 (id BIGSERIAL PRIMARY KEY, email TEXT NOT NULL, reason TEXT NOT NULL DEFAULT '');\n\
         CREATE UNIQUE INDEX idx_t7_email ON t7 (email);",
    )
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO t7 (email, reason) VALUES ($1, $2)\
         ON CONFLICT (email) DO UPDATE SET reason = $2",
    )
    .bind("a@x")
    .bind("first")
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO t7 (email, reason) VALUES ($1, $2)\
         ON CONFLICT (email) DO UPDATE SET reason = $2",
    )
    .bind("a@x")
    .bind("second")
    .execute(&p)
    .await
    .unwrap();
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT email, reason FROM t7 ORDER BY email")
        .fetch_all(&p)
        .await
        .unwrap();
    assert_eq!(rows, vec![("a@x".into(), "second".into())]);
}
