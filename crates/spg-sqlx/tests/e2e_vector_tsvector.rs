//! v7.17.0 Phase 3.P0-68 — `Vec<f32>` ↔ pgvector `VECTOR(N)`
//! round-trip and `String` Decode for `TSVECTOR`.
//!
//! Three encodings on the column side (default f32, `USING SQ8`,
//! `USING HALF`) all decode through the same Rust target.

use spg_sqlx::{Kind, SpgConnectOptions};
use sqlx::ConnectOptions;
use sqlx::{Column, Executor, Row, TypeInfo};

#[tokio::test]
async fn vector_column_round_trips_through_bind() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE embeds (id INT NOT NULL, v VECTOR(3))")
        .execute(&mut conn)
        .await
        .unwrap();
    let v1: Vec<f32> = vec![1.0, 2.0, 3.0];
    let v2: Vec<f32> = vec![-0.5, 0.25, 100.125];
    sqlx::query("INSERT INTO embeds VALUES (1, $1), (2, $2)")
        .bind(&v1)
        .bind(&v2)
        .execute(&mut conn)
        .await
        .unwrap();
    let rows: Vec<(i32, Vec<f32>)> =
        sqlx::query_as("SELECT id, v FROM embeds ORDER BY id")
            .fetch_all(&mut conn)
            .await
            .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1, v1);
    assert_eq!(rows[1].1, v2);
}

#[tokio::test]
async fn vector_column_decodes_as_canonical_string() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE embeds (v VECTOR(3))")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO embeds VALUES ('[1, 2.5, -3]')")
        .execute(&mut conn)
        .await
        .unwrap();
    let (rendered,): (String,) = sqlx::query_as("SELECT v FROM embeds")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(rendered, "[1, 2.5, -3]");
}

#[tokio::test]
async fn vector_column_type_info_surfaces_kind_vector() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE e (v VECTOR(2))")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO e VALUES ('[0, 0]')")
        .execute(&mut conn)
        .await
        .unwrap();
    let row = sqlx::query("SELECT v FROM e")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(row.columns()[0].type_info().kind(), Kind::Vector);
    assert_eq!(row.columns()[0].type_info().name(), "VECTOR");
}

#[tokio::test]
async fn vector_sq8_storage_dequantises_into_vec_f32() {
    // `USING SQ8` storage variant — Decode<Vec<f32>> must
    // dequantise transparently. Round-trip is approximate (8-bit
    // quantisation introduces small error), so we check the
    // length and the per-component error bound matches what
    // `spg_storage::quantize::dequantize` produces.
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE e (v VECTOR(4) USING SQ8)")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO e VALUES ('[0, 1, 2, 3]')")
        .execute(&mut conn)
        .await
        .unwrap();
    let (got,): (Vec<f32>,) = sqlx::query_as("SELECT v FROM e")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(got.len(), 4);
    // 8-bit quantisation over [0, 3] gives per-step ~0.012.
    // Allow a generous epsilon — we only assert dequant ran.
    let expected = [0.0, 1.0, 2.0, 3.0];
    for (g, e) in got.iter().zip(expected.iter()) {
        assert!((g - e).abs() < 0.02, "sq8 dequant: got {g}, expected ~{e}");
    }
}

#[tokio::test]
async fn vector_half_storage_dequantises_into_vec_f32() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE e (v VECTOR(3) USING HALF)")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO e VALUES ('[0.5, 1.5, 2.5]')")
        .execute(&mut conn)
        .await
        .unwrap();
    let (got,): (Vec<f32>,) = sqlx::query_as("SELECT v FROM e")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(got.len(), 3);
    // half-precision floats: ~3-4 decimal digits of precision —
    // the test values round-trip exactly in this range.
    assert_eq!(got, vec![0.5_f32, 1.5, 2.5]);
}

#[tokio::test]
async fn vector_describe_resolves_column_kind() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE e (v VECTOR(8))")
        .execute(&mut conn)
        .await
        .unwrap();
    let d = conn.describe("SELECT v FROM e").await.unwrap();
    assert_eq!(d.columns.len(), 1);
    assert_eq!(d.columns[0].type_info().kind(), Kind::Vector);
}

#[tokio::test]
async fn tsvector_column_decodes_as_canonical_string() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE docs (id INT NOT NULL, body TSVECTOR)")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO docs VALUES (1, to_tsvector('the quick brown fox'))")
        .execute(&mut conn)
        .await
        .unwrap();
    let (rendered,): (String,) = sqlx::query_as("SELECT body FROM docs")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert!(
        rendered.contains("'fox'") && rendered.contains("'quick'"),
        "expected canonical tsvector form, got {rendered}"
    );
}

#[tokio::test]
async fn tsvector_column_type_info_surfaces_kind_tsvector() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE d (body TSVECTOR)")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO d VALUES (to_tsvector('hi'))")
        .execute(&mut conn)
        .await
        .unwrap();
    let row = sqlx::query("SELECT body FROM d")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(row.columns()[0].type_info().kind(), Kind::TsVector);
    assert_eq!(row.columns()[0].type_info().name(), "TSVECTOR");
}

#[tokio::test]
async fn vec_f32_decode_rejects_non_vector_cells() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE t (name TEXT)")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO t VALUES ('hello')")
        .execute(&mut conn)
        .await
        .unwrap();
    let r: Result<(Vec<f32>,), _> = sqlx::query_as("SELECT name FROM t")
        .fetch_one(&mut conn)
        .await;
    assert!(
        r.is_err(),
        "TEXT cell must not silently decode as Vec<f32>"
    );
}

#[tokio::test]
async fn null_vector_decodes_as_none() {
    let mut conn = SpgConnectOptions::in_memory().connect().await.unwrap();
    sqlx::query("CREATE TABLE e (v VECTOR(3))")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO e VALUES (NULL)")
        .execute(&mut conn)
        .await
        .unwrap();
    let (got,): (Option<Vec<f32>>,) = sqlx::query_as("SELECT v FROM e")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert!(got.is_none());
}
