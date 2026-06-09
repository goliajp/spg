//! v7.17.0 Phase 3.P0-67 — NUMERIC / DECIMAL sqlx bridge.
//!
//! Two surfaces verified:
//!   * Unconditional `Decode<String>` for NUMERIC cells — every
//!     sqlx consumer can read NUMERIC columns as canonical text
//!     without enabling the `bigdecimal` feature.
//!   * `BigDecimal` Encode/Decode round-trip under the
//!     `bigdecimal` feature — full arbitrary-precision math
//!     bridge that matches sqlx-postgres's surface.

use bigdecimal::BigDecimal;
use spg_sqlx::{Kind, SpgConnectOptions};
use sqlx::ConnectOptions;
use sqlx::{Column, Executor, Row, TypeInfo};
use std::str::FromStr;

#[tokio::test]
async fn numeric_column_decodes_as_canonical_string() {
    // Universal path — no feature flag needed. Every sqlx user
    // gets readable NUMERIC text out of the box.
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE invoice (amount NUMERIC(10, 2))")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO invoice VALUES (1234.56), (-0.50), (0)")
        .execute(&mut conn)
        .await
        .unwrap();
    let rows: Vec<(String,)> = sqlx::query_as("SELECT amount FROM invoice ORDER BY amount")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    let amounts: Vec<String> = rows.into_iter().map(|(s,)| s).collect();
    assert_eq!(amounts, vec!["-0.50", "0.00", "1234.56"]);
}

#[tokio::test]
async fn numeric_column_type_info_surfaces_kind_numeric() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE t (amount NUMERIC(8, 3))")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO t VALUES (1.500)")
        .execute(&mut conn)
        .await
        .unwrap();
    let row = sqlx::query("SELECT amount FROM t")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(row.columns()[0].type_info().kind(), Kind::Numeric);
    assert_eq!(row.columns()[0].type_info().name(), "NUMERIC");
}

#[tokio::test]
async fn describe_resolves_numeric_columns_for_sqlx_query_macro() {
    // Regression for P0-66 + P0-67 cooperation — `sqlx::query!()`
    // calls describe to learn column types at compile time. With
    // NUMERIC bridged, the describe path now surfaces Kind::Numeric
    // so the macro can pick the right Rust target.
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE bal (amount NUMERIC(20, 5))")
        .execute(&mut conn)
        .await
        .unwrap();
    let d = conn
        .describe("SELECT amount FROM bal")
        .await
        .unwrap();
    assert_eq!(d.columns.len(), 1);
    assert_eq!(d.columns[0].type_info().kind(), Kind::Numeric);
}

#[tokio::test]
async fn bigdecimal_round_trip_preserves_value() {
    // Encode + Decode path under the bigdecimal feature.
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE t (id INT NOT NULL, amount NUMERIC(20, 5))")
        .execute(&mut conn)
        .await
        .unwrap();
    let v1 = BigDecimal::from_str("12345.67890").unwrap();
    let v2 = BigDecimal::from_str("-987654321.00001").unwrap();
    let v3 = BigDecimal::from_str("0.00000").unwrap();
    sqlx::query("INSERT INTO t VALUES (1, $1), (2, $2), (3, $3)")
        .bind(&v1)
        .bind(&v2)
        .bind(&v3)
        .execute(&mut conn)
        .await
        .unwrap();
    let rows: Vec<(i32, BigDecimal)> = sqlx::query_as("SELECT id, amount FROM t ORDER BY id")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].1, v1);
    assert_eq!(rows[1].1, v2);
    // BigDecimal '0' vs '0.00000' are NOT bit-equal — they
    // differ in trailing-zero count. SPG always returns with
    // the column's declared scale, so the round-trip exposes
    // the canonical scale-5 form. Verify by re-parsing.
    assert_eq!(rows[2].1, BigDecimal::from_str("0.00000").unwrap());
}

#[tokio::test]
async fn bigdecimal_decode_widens_small_ints() {
    // BigDecimal Decode is generous: SMALLINT / INT / BIGINT
    // columns can decode to BigDecimal too. Mirrors PG wire
    // behaviour where integer columns are valid NUMERIC inputs.
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE t (a INT NOT NULL, b BIGINT NOT NULL, c SMALLINT NOT NULL)")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO t VALUES (42, 9000000000, 7)")
        .execute(&mut conn)
        .await
        .unwrap();
    let row: (BigDecimal, BigDecimal, BigDecimal) =
        sqlx::query_as("SELECT a, b, c FROM t")
            .fetch_one(&mut conn)
            .await
            .unwrap();
    assert_eq!(row.0, BigDecimal::from(42));
    assert_eq!(row.1, BigDecimal::from(9_000_000_000i64));
    assert_eq!(row.2, BigDecimal::from(7));
}

#[tokio::test]
async fn bigdecimal_decode_rejects_non_numeric_cells() {
    // Decode must error on incompatible cells — e.g. TEXT — so
    // the user sees a clear message instead of a silent zero.
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE t (name TEXT)")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO t VALUES ('hello')")
        .execute(&mut conn)
        .await
        .unwrap();
    let r: Result<(BigDecimal,), _> = sqlx::query_as("SELECT name FROM t")
        .fetch_one(&mut conn)
        .await;
    assert!(r.is_err(), "TEXT cell must not silently decode as BigDecimal");
}

#[tokio::test]
async fn bigdecimal_encode_negative_exponent_folds_into_mantissa() {
    // BigDecimal `1.5e3` parses as mantissa=15 exponent=-2 (i.e.
    // 1500). SPG NUMERIC carries (scaled, scale) with scale ≥ 0,
    // so the encode path must fold the negative exponent into
    // the mantissa before storing.
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE t (amount NUMERIC(10, 0))")
        .execute(&mut conn)
        .await
        .unwrap();
    let v = BigDecimal::from_str("1.5e3").unwrap(); // 1500
    sqlx::query("INSERT INTO t VALUES ($1)")
        .bind(&v)
        .execute(&mut conn)
        .await
        .unwrap();
    let (got,): (BigDecimal,) = sqlx::query_as("SELECT amount FROM t")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(got, BigDecimal::from(1500));
}

#[tokio::test]
async fn bigdecimal_encode_overflow_surfaces_as_encode_error() {
    // Mantissa beyond i128 (precision > 38) must produce an
    // Encode error, not a silent truncate. Pick a value the
    // i128 range can't hold.
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE t (amount NUMERIC)")
        .execute(&mut conn)
        .await
        .unwrap();
    // 40-digit mantissa — outside i128 (max ~3.4e38).
    let huge = BigDecimal::from_str("1234567890123456789012345678901234567890").unwrap();
    let r = sqlx::query("INSERT INTO t VALUES ($1)")
        .bind(&huge)
        .execute(&mut conn)
        .await;
    assert!(r.is_err(), "i128 overflow must surface as encode error");
}

#[tokio::test]
async fn null_numeric_decodes_as_none() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE t (amount NUMERIC(10, 2))")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO t VALUES (NULL)")
        .execute(&mut conn)
        .await
        .unwrap();
    let (got,): (Option<BigDecimal>,) = sqlx::query_as("SELECT amount FROM t")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert!(got.is_none());
}
