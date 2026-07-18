//! v7.11.3 — PG-customer parity regression suite.
//!
//! Mirrors the gap list mailrs raised in D-cutover. Every test
//! corresponds to a single PG idiom that arrived as feedback; if
//! it ever regresses, real customers break.
//!
//! Categories:
//!   - Closed gaps assert success (the idiom must keep working).
//!   - Open gaps are `#[ignore]` with a `// TODO(vN.M):` marker.
//!     Run with `cargo test -p spg-engine -- --ignored` to see
//!     what still doesn't work end-to-end.

use spg_engine::Engine;
use spg_storage::Value;

fn fake_clock() -> i64 {
    // Pinned wall-clock — 2026-06-04T00:00:00Z in unix micros.
    1_780_531_200_000_000
}

fn eng() -> Engine {
    Engine::new().with_clock(fake_clock)
}

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn first_value(e: &mut Engine, sql: &str) -> Value<'static> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    match r {
        spg_engine::QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .next()
            .map(|mut r| r.values.remove(0))
            .expect("at least one row"),
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn explain_text(e: &mut Engine, sql: &str) -> String {
    let stmt = format!("EXPLAIN {sql}");
    let r = e
        .execute(&stmt)
        .unwrap_or_else(|err| panic!("{stmt}: {err:?}"));
    match r {
        spg_engine::QueryResult::Rows { rows, .. } => rows
            .iter()
            .flat_map(|r| r.values.iter())
            .map(|v| match v {
                Value::Text(s) => s.to_string(),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => panic!("expected EXPLAIN to return Rows, got {other:?}"),
    }
}

// G-CRIT-1 — parameterised LIMIT $n (closed v7.9.24).
#[test]
fn limit_placeholder_parses() {
    let mut e = eng();
    ok(&mut e, "CREATE TABLE t (id INT NOT NULL)");
    ok(&mut e, "INSERT INTO t VALUES (1), (2), (3)");
    let _ = e
        .execute("SELECT id FROM t ORDER BY id LIMIT $1")
        .expect("LIMIT $1 must parse");
}

// G-CRIT-2 — NOW() / CURRENT_TIMESTAMP / CURRENT_DATE + INTERVAL
// (closed v7.11.3 by wiring Engine::with_clock into embedded).
#[test]
fn now_minus_interval_30_days() {
    let mut e = eng();
    ok(
        &mut e,
        "CREATE TABLE m (id INT NOT NULL, created_at TIMESTAMP NOT NULL)",
    );
    ok(&mut e, "INSERT INTO m VALUES (1, '2026-01-01 00:00:00')");
    let _ = first_value(
        &mut e,
        "SELECT COUNT(*) FROM m WHERE created_at > NOW() - INTERVAL '30 days'",
    );
}

#[test]
fn current_timestamp_minus_interval() {
    let mut e = eng();
    ok(
        &mut e,
        "CREATE TABLE m (id INT NOT NULL, ts TIMESTAMP NOT NULL)",
    );
    ok(&mut e, "INSERT INTO m VALUES (1, '2026-01-01 00:00:00')");
    let _ = first_value(
        &mut e,
        "SELECT COUNT(*) FROM m WHERE ts > CURRENT_TIMESTAMP - INTERVAL '30 days'",
    );
}

#[test]
fn current_date_works() {
    let mut e = eng();
    let v = first_value(&mut e, "SELECT CURRENT_DATE");
    assert!(matches!(v, Value::Date(_)));
}

#[test]
fn bare_interval_literal_evaluates() {
    let mut e = eng();
    let v = first_value(&mut e, "SELECT INTERVAL '30 days'");
    // v7.37.5 β — `'30 days'` lands in `days` (PG byte-equal),
    // not micros.
    assert!(matches!(
        v,
        Value::Interval {
            months: 0,
            days: 30,
            micros: 0,
        }
    ));
}

// G-CRIT-4 — pgvector ivfflat as HNSW alias (closed v7.11.3).
#[test]
fn ivfflat_index_accepted() {
    let mut e = eng();
    ok(
        &mut e,
        "CREATE TABLE emb (id INT NOT NULL, v VECTOR(8) NOT NULL)",
    );
    ok(
        &mut e,
        "CREATE INDEX idx_emb ON emb USING ivfflat (v) WITH (lists = 20)",
    );
}

#[test]
fn hnsw_with_storage_params_accepted() {
    let mut e = eng();
    ok(
        &mut e,
        "CREATE TABLE emb (id INT NOT NULL, v VECTOR(8) NOT NULL)",
    );
    ok(
        &mut e,
        "CREATE INDEX idx_emb ON emb USING hnsw (v) WITH (m = 16, ef_construction = 64)",
    );
}

// G-CRIT-5 — CREATE EXTENSION as no-op (closed v7.9.15).
#[test]
fn create_extension_vector_no_op() {
    let mut e = eng();
    ok(&mut e, "CREATE EXTENSION IF NOT EXISTS vector");
    ok(&mut e, "CREATE EXTENSION vector");
    ok(
        &mut e,
        "CREATE EXTENSION IF NOT EXISTS pgvector WITH SCHEMA public",
    );
}

// G-CRIT-6 — Inline `id BIGSERIAL PRIMARY KEY` (closed v7.9.13).
#[test]
fn bigserial_inline_primary_key() {
    let mut e = eng();
    ok(
        &mut e,
        "CREATE TABLE foo (id BIGSERIAL PRIMARY KEY, body TEXT NOT NULL)",
    );
    ok(&mut e, "INSERT INTO foo (body) VALUES ('a'), ('b')");
    let v = first_value(&mut e, "SELECT COUNT(*) FROM foo");
    assert!(matches!(v, Value::BigInt(2) | Value::Int(2)));
}

// G-CRIT-7 — Multi-column index picker uses leading column from
// AND-composite WHERE (closed v7.11.3).
#[test]
fn multi_column_index_picker_recurses_into_and() {
    let mut e = eng();
    ok(
        &mut e,
        "CREATE TABLE m (id INT NOT NULL, created_at TIMESTAMP NOT NULL)",
    );
    ok(&mut e, "CREATE INDEX idx_thread ON m (id, created_at)");
    ok(&mut e, "INSERT INTO m VALUES (1, '2026-01-01 00:00:00')");
    let text = explain_text(
        &mut e,
        "SELECT id FROM m WHERE id = 1 AND created_at > '2025-01-01'::TIMESTAMP",
    );
    // v7.39 (round 224) — PG-shaped: an index pick renders as Index Scan.
    assert!(
        text.contains("Index Scan using"),
        "EXPLAIN expected an Index Scan but got:\n{text}"
    );
}

#[test]
fn pk_index_picker_used_with_and_predicate() {
    let mut e = eng();
    ok(
        &mut e,
        "CREATE TABLE foo (id INT NOT NULL PRIMARY KEY, body TEXT NOT NULL)",
    );
    ok(&mut e, "INSERT INTO foo VALUES (1, 'a'), (2, 'b')");
    let text = explain_text(
        &mut e,
        "SELECT body FROM foo WHERE id = 1 AND body LIKE '%a%'",
    );
    assert!(
        text.contains("Index Scan using foo_pkey"),
        "PK index should be picked under AND-composite WHERE; got:\n{text}"
    );
}

// G-NICE-1 — pgvector opclass on HNSW (closed v7.9.22).
#[test]
fn hnsw_with_vector_cosine_ops_opclass() {
    let mut e = eng();
    ok(
        &mut e,
        "CREATE TABLE emb (id INT NOT NULL, v VECTOR(8) NOT NULL)",
    );
    ok(
        &mut e,
        "CREATE INDEX idx_emb ON emb USING hnsw (v vector_cosine_ops)",
    );
}

// v7.12.0 — G-CRIT-3 entry: tsvector / tsquery column types load.
// Full `@@` / `to_tsvector` / GIN / triggers land across v7.12.1–7.
#[test]
fn tsvector_column_type_accepted() {
    let mut e = eng();
    ok(
        &mut e,
        "CREATE TABLE m (id INT NOT NULL, v tsvector NOT NULL)",
    );
    // tsquery column too — both types should parse cleanly.
    ok(
        &mut e,
        "CREATE TABLE q (id INT NOT NULL, query tsquery NOT NULL)",
    );
}

// v7.12.0 — `'..'::tsvector` cast (pg_dump form). Round-trip of a
// dumped-row literal back through SELECT must preserve the lexeme set.
#[test]
fn tsvector_cast_literal_round_trip() {
    let mut e = eng();
    ok(
        &mut e,
        "CREATE TABLE m (id INT NOT NULL, v tsvector NOT NULL)",
    );
    ok(
        &mut e,
        "INSERT INTO m VALUES (1, 'cat:1 fat:2 rat:3,5A'::tsvector)",
    );
    let cell = first_value(&mut e, "SELECT v FROM m WHERE id = 1");
    let rendered = match cell {
        Value::TsVector(items) => spg_engine::eval::format_tsvector(&items),
        other => panic!("expected tsvector, got {other:?}"),
    };
    assert_eq!(rendered, "'cat':1 'fat':2 'rat':3,5A");
}

// v7.12.0 — `'..'::tsquery` cast (`to_tsquery` literal surface).
#[test]
fn tsquery_cast_literal_round_trip() {
    let mut e = eng();
    ok(
        &mut e,
        "CREATE TABLE q (id INT NOT NULL, query tsquery NOT NULL)",
    );
    ok(
        &mut e,
        "INSERT INTO q VALUES (1, 'cat & dog | !fish'::tsquery)",
    );
    let cell = first_value(&mut e, "SELECT query FROM q WHERE id = 1");
    let rendered = match cell {
        Value::TsQuery(ast) => spg_engine::eval::format_tsquery(&ast),
        other => panic!("expected tsquery, got {other:?}"),
    };
    // Or-of-And-of-Not: parser builds `(cat & dog) | !fish`.
    assert_eq!(rendered, "'cat' & 'dog' | !'fish'");
}
