//! v7.37.43-T4.5 — `jsonb_each_text(<expr>)` SRF acceptance suite.
//!
//! Sentori migration 0067 backfill shape:
//!   SELECT ... FROM events e
//!     JOIN ... ois ...
//!     CROSS JOIN LATERAL jsonb_each_text(
//!         COALESCE(e.payload->'user'->'linkHashes', '{}'::jsonb)
//!     ) AS kv(key, value)
//!   WHERE length(kv.value) = 64
//!
//! PG semantics. `jsonb_each_text(jsonb)` is a set-returning
//! function: for each (key, value) pair in the object argument
//! it emits one row whose key column is the literal key and
//! value column is the JSON value rendered as text. NULL input
//! and the empty object both produce 0 rows; non-object input
//! raises an error (matching PG).

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn execute(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("execute({sql}): {err:?}"));
}

fn execute_rows(e: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("execute({sql}): {err:?}"))
    {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("expected Rows from {sql}, got {other:?}"),
    }
}

// ─── T4.5-α — uncorrelated FROM jsonb_each_text(literal) ─────────

#[test]
fn jsonb_each_text_from_literal_object_yields_pair_rows() {
    let mut e = Engine::new();
    let rows = execute_rows(
        &mut e,
        "SELECT key, value FROM jsonb_each_text('{\"a\":\"1\",\"b\":\"2\"}'::jsonb) ORDER BY key",
    );
    assert_eq!(rows.len(), 2, "two pairs");
    assert_eq!(rows[0][0], Value::text("a".to_string()));
    assert_eq!(rows[0][1], Value::text("1".to_string()));
    assert_eq!(rows[1][0], Value::text("b".to_string()));
    assert_eq!(rows[1][1], Value::text("2".to_string()));
}

// ─── T4.5-β — empty object yields zero rows ───────────────────────

#[test]
fn jsonb_each_text_empty_object_yields_no_rows() {
    let mut e = Engine::new();
    let rows = execute_rows(
        &mut e,
        "SELECT key, value FROM jsonb_each_text('{}'::jsonb)",
    );
    assert_eq!(rows.len(), 0);
}

// ─── T4.5-γ — NULL input yields zero rows ─────────────────────────

#[test]
fn jsonb_each_text_null_arg_yields_no_rows() {
    let mut e = Engine::new();
    let rows = execute_rows(
        &mut e,
        "SELECT key, value FROM jsonb_each_text(NULL::jsonb)",
    );
    assert_eq!(rows.len(), 0);
}

// ─── T4.5-ε — sentori 0067 backfill dogfood shape ─────────────────

#[test]
fn sentori_0067_style_lateral_jsonb_each_text_filter_pattern() {
    let mut e = Engine::new();
    // events.payload -> 'user' -> 'linkHashes' is a JSONB object of
    // (label -> hash) pairs; sentori 0067 walks those pairs via
    // LATERAL jsonb_each_text and writes one fingerprint row per
    // (event, scope, key_type) tuple.
    execute(
        &mut e,
        "CREATE TABLE events (id INT PRIMARY KEY, payload JSONB)",
    );
    execute(
        &mut e,
        "CREATE TABLE fingerprints (event_id INT, key_type TEXT, value TEXT)",
    );
    execute(
        &mut e,
        "INSERT INTO events VALUES \
             (1, '{\"user\":{\"linkHashes\":{\"email\":\"abc123\",\"phone\":\"def456\"}}}'::jsonb), \
             (2, '{\"user\":{\"linkHashes\":{}}}'::jsonb), \
             (3, '{\"user\":{}}'::jsonb)",
    );
    // The actual sentori migration uses COALESCE around the
    // payload path and length filter; we mirror the shape here.
    execute(
        &mut e,
        "INSERT INTO fingerprints (event_id, key_type, value) \
         SELECT e.id, kv.key, kv.value \
           FROM events e \
           CROSS JOIN LATERAL jsonb_each_text( \
             COALESCE(e.payload->'user'->'linkHashes', '{}'::jsonb) \
           ) AS kv(key, value)",
    );
    let fp = execute_rows(
        &mut e,
        "SELECT event_id, key_type, value FROM fingerprints \
            ORDER BY event_id, key_type",
    );
    assert_eq!(
        fp.len(),
        2,
        "only event 1 contributed pairs (email + phone)"
    );
    assert_eq!(fp[0][0], Value::Int(1));
    assert_eq!(fp[0][1], Value::text("email".to_string()));
    assert_eq!(fp[0][2], Value::text("abc123".to_string()));
    assert_eq!(fp[1][0], Value::Int(1));
    assert_eq!(fp[1][1], Value::text("phone".to_string()));
    assert_eq!(fp[1][2], Value::text("def456".to_string()));
}

// ─── T4.5-δ — CROSS JOIN LATERAL jsonb_each_text(t.col) (sentori 0067) ──

#[test]
fn cross_join_lateral_jsonb_each_text_correlates_with_parent_row() {
    let mut e = Engine::new();
    execute(
        &mut e,
        "CREATE TABLE docs (id INT PRIMARY KEY, payload JSONB)",
    );
    execute(
        &mut e,
        "INSERT INTO docs VALUES \
             (1, '{\"a\":\"one\",\"b\":\"two\"}'::jsonb), \
             (2, '{\"c\":\"three\"}'::jsonb), \
             (3, '{}'::jsonb)",
    );

    let rows = execute_rows(
        &mut e,
        "SELECT d.id, kv.key, kv.value \
           FROM docs d \
           CROSS JOIN LATERAL jsonb_each_text(d.payload) AS kv(key, value) \
           ORDER BY d.id, kv.key",
    );
    assert_eq!(
        rows.len(),
        3,
        "row 1 contributes 2 pairs, row 2 contributes 1, row 3 contributes 0"
    );
    // row 1: (1, a, one), (1, b, two)
    assert_eq!(rows[0][0], Value::Int(1));
    assert_eq!(rows[0][1], Value::text("a".to_string()));
    assert_eq!(rows[0][2], Value::text("one".to_string()));
    assert_eq!(rows[1][0], Value::Int(1));
    assert_eq!(rows[1][1], Value::text("b".to_string()));
    assert_eq!(rows[1][2], Value::text("two".to_string()));
    // row 2: (2, c, three)
    assert_eq!(rows[2][0], Value::Int(2));
    assert_eq!(rows[2][1], Value::text("c".to_string()));
    assert_eq!(rows[2][2], Value::text("three".to_string()));
}
