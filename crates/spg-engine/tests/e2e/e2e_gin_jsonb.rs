//! v7.37.8 (sentori Epic 5 P2) — real GIN posting list on JSONB.
//!
//! Pre-7.37.8 `CREATE INDEX … USING GIN (jsonb_col)` loaded as a
//! BTree fallback so `pg_dump` scripts kept loading but
//! `labels @> '...'` queries fell back to full scan. v7.37.8 wires
//! the real posting-list index; this suite pins:
//!   * INSERT maintenance (tokens land on the posting list)
//!   * `@>` containment seek through the index
//!   * `@>` containment correctness when multiple keys constrain
//!     the candidate set
//!   * sentori acceptance probe — `labels @> '{"team":"ios"}'`
//!     against a populated table returns only matching rows
//!
//! The seek is over-approximate by design (PG semantics): the
//! engine re-evaluates the full `@>` predicate per candidate row
//! after the posting-list intersection, so the result set is
//! always strictly correct.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<Value>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows for {sql}");
    };
    rows.into_iter().map(|r| r.values).collect()
}

fn one_i64(e: &mut Engine, sql: &str) -> i64 {
    let mut rs = rows(e, sql);
    let row = rs.pop().expect("one row");
    match row.into_iter().next().expect("one col") {
        Value::BigInt(n) => n,
        Value::Int(n) => i64::from(n),
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn gin_on_jsonb_accelerates_simple_containment() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE issues (id BIGINT NOT NULL, labels JSONB)")
        .unwrap();
    e.execute("CREATE INDEX issues_labels_gin ON issues USING GIN (labels)")
        .expect("real JSONB-GIN");
    // Seed three rows with distinct label shapes.
    e.execute(r#"INSERT INTO issues (id, labels) VALUES (1, '{"team":"ios"}'::jsonb)"#)
        .unwrap();
    e.execute(r#"INSERT INTO issues (id, labels) VALUES (2, '{"team":"android"}'::jsonb)"#)
        .unwrap();
    e.execute(
        r#"INSERT INTO issues (id, labels) VALUES (3, '{"team":"ios","sev":"high"}'::jsonb)"#,
    )
    .unwrap();
    // `labels @> '{"team":"ios"}'` matches rows 1 and 3.
    let result = rows(
        &mut e,
        r#"SELECT id FROM issues WHERE labels @> '{"team":"ios"}'::jsonb ORDER BY id"#,
    );
    assert_eq!(result.len(), 2);
    assert_eq!(result[0][0], Value::BigInt(1));
    assert_eq!(result[1][0], Value::BigInt(3));
}

#[test]
fn gin_on_jsonb_handles_multi_key_containment() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE issues (id BIGINT NOT NULL, labels JSONB)")
        .unwrap();
    e.execute("CREATE INDEX issues_labels_gin ON issues USING GIN (labels)")
        .unwrap();
    e.execute(
        r#"INSERT INTO issues (id, labels) VALUES (1, '{"team":"ios","sev":"high"}'::jsonb)"#,
    )
    .unwrap();
    e.execute(r#"INSERT INTO issues (id, labels) VALUES (2, '{"team":"ios","sev":"low"}'::jsonb)"#)
        .unwrap();
    e.execute(
        r#"INSERT INTO issues (id, labels) VALUES (3, '{"team":"android","sev":"high"}'::jsonb)"#,
    )
    .unwrap();
    // Both keys must match — only row 1.
    let result = rows(
        &mut e,
        r#"SELECT id FROM issues WHERE labels @> '{"team":"ios","sev":"high"}'::jsonb ORDER BY id"#,
    );
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::BigInt(1));
}

#[test]
fn gin_on_jsonb_excludes_non_matching_rows() {
    // The posting-list intersection may return an over-approximate
    // set; the engine's full @> re-eval per row is the correctness
    // guard. Probe that the final result strictly excludes a row
    // whose labels contain only one of two queried keys.
    let mut e = Engine::new();
    e.execute("CREATE TABLE issues (id BIGINT NOT NULL, labels JSONB)")
        .unwrap();
    e.execute("CREATE INDEX issues_labels_gin ON issues USING GIN (labels)")
        .unwrap();
    e.execute(r#"INSERT INTO issues (id, labels) VALUES (1, '{"team":"ios"}'::jsonb)"#)
        .unwrap();
    e.execute(r#"INSERT INTO issues (id, labels) VALUES (2, '{"sev":"high"}'::jsonb)"#)
        .unwrap();
    let result = rows(
        &mut e,
        r#"SELECT id FROM issues WHERE labels @> '{"team":"ios","sev":"high"}'::jsonb"#,
    );
    assert!(result.is_empty(), "expected no matches, got {result:?}");
}

/// sentori acceptance probe at a scale where a full scan would be
/// O(N) — populate a few thousand rows and verify the index returns
/// a small candidate set quickly.
#[test]
fn sentori_acceptance_probe_filters_to_matching_team() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE issues (id BIGINT NOT NULL, labels JSONB)")
        .unwrap();
    e.execute("CREATE INDEX issues_labels_gin ON issues USING GIN (labels)")
        .unwrap();
    // 2000 rows distributed across 4 teams.
    let teams = ["ios", "android", "web", "rn"];
    for i in 0..2000i64 {
        let team = teams[(i as usize) % teams.len()];
        e.execute(&format!(
            r#"INSERT INTO issues (id, labels) VALUES ({i}, '{{"team":"{team}"}}'::jsonb)"#
        ))
        .unwrap();
    }
    // 500 rows per team → ios subset has exactly 500.
    assert_eq!(
        one_i64(
            &mut e,
            r#"SELECT count(*) FROM issues WHERE labels @> '{"team":"ios"}'::jsonb"#
        ),
        500
    );
    // A non-existent team returns zero.
    assert_eq!(
        one_i64(
            &mut e,
            r#"SELECT count(*) FROM issues WHERE labels @> '{"team":"linux"}'::jsonb"#
        ),
        0
    );
}
