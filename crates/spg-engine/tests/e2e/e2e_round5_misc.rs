//! v7.13.0 — miscellaneous mailrs round-5 gap fixes:
//! G5 (gin_trgm_ops opclass token), G6 (DOUBLE PRECISION type),
//! G9 (CASE WHEN in expressions).

use spg_engine::Engine;

// ── G5 — gin_trgm_ops + PG built-in opclass tokens ────────────

#[test]
fn create_index_using_gin_with_trgm_opclass_is_accepted() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE docs (id INT NOT NULL, clean_text TEXT)")
        .unwrap();
    eng.execute("CREATE INDEX docs_clean_text_idx ON docs USING gin (clean_text gin_trgm_ops)")
        .unwrap();
}

#[test]
fn create_index_with_text_pattern_ops_is_accepted() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (name TEXT NOT NULL)").unwrap();
    eng.execute("CREATE INDEX t_name_lower ON t (name text_pattern_ops)")
        .unwrap();
}

// ── G6 — DOUBLE PRECISION + FLOAT8 ────────────────────────────

#[test]
fn double_precision_accepted_as_float_synonym() {
    let mut eng = Engine::new();
    eng.execute(
        "CREATE TABLE scores (id INT NOT NULL, confidence DOUBLE PRECISION NOT NULL DEFAULT 0)",
    )
    .unwrap();
    eng.execute("INSERT INTO scores VALUES (1, 0.42)").unwrap();
    let table = eng.catalog().get("scores").expect("table present");
    assert_eq!(table.rows().len(), 1);
}

#[test]
fn float8_accepted_as_float_synonym() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (x FLOAT8 NOT NULL)").unwrap();
    eng.execute("INSERT INTO t VALUES (3.14)").unwrap();
}

// ── G9 — CASE WHEN in UPDATE / SELECT expression context ──────

#[test]
fn case_when_in_update_set_expression() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE msgs (id INT NOT NULL, message_id TEXT NOT NULL)")
        .unwrap();
    eng.execute("INSERT INTO msgs VALUES (1, ''), (2, 'real-id')")
        .unwrap();
    eng.execute(
        "UPDATE msgs SET message_id = CASE WHEN message_id = '' THEN 'generated' ELSE message_id END",
    )
    .unwrap();
    // Read back through SQL, not physical row slots — under the
    // mvcc-inplace-on verification feature an UPDATE tombstones the old
    // version and appends the new one, so slot 0 is the dead version.
    let spg_engine::QueryResult::Rows { rows, .. } = eng
        .execute("SELECT message_id FROM msgs ORDER BY id")
        .unwrap()
    else {
        panic!("rows");
    };
    assert_eq!(rows.len(), 2);
    assert!(
        matches!(&rows[0].values[0], spg_storage::Value::Text(s) if s == "generated"),
        "row id=1 should have generated id, got {:?}",
        rows[0].values[0]
    );
    assert!(
        matches!(&rows[1].values[0], spg_storage::Value::Text(s) if s == "real-id"),
        "row id=2 should keep its id, got {:?}",
        rows[1].values[0]
    );
}

#[test]
fn case_when_in_select_searched_form() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (n INT NOT NULL)").unwrap();
    eng.execute("INSERT INTO t VALUES (1), (5), (10)").unwrap();
    let r = eng
        .execute("SELECT CASE WHEN n < 5 THEN 'small' WHEN n < 10 THEN 'mid' ELSE 'big' END FROM t")
        .unwrap();
    let rows = match r {
        spg_engine::QueryResult::Rows { rows, .. } => rows,
        _ => panic!("expected rows"),
    };
    assert_eq!(rows.len(), 3);
    let to_text = |v: &spg_storage::Value| match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected text, got {other:?}"),
    };
    assert_eq!(to_text(&rows[0].values[0]), "small");
    assert_eq!(to_text(&rows[1].values[0]), "mid");
    assert_eq!(to_text(&rows[2].values[0]), "big");
}

#[test]
fn case_when_no_else_returns_null() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (n INT NOT NULL)").unwrap();
    eng.execute("INSERT INTO t VALUES (1), (5)").unwrap();
    let r = eng
        .execute("SELECT CASE WHEN n > 100 THEN 'big' END FROM t")
        .unwrap();
    let rows = match r {
        spg_engine::QueryResult::Rows { rows, .. } => rows,
        _ => panic!("expected rows"),
    };
    assert_eq!(rows.len(), 2);
    assert!(rows[0].values[0].is_null());
    assert!(rows[1].values[0].is_null());
}

#[test]
fn case_when_simple_form_with_operand() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (kind TEXT NOT NULL)").unwrap();
    eng.execute("INSERT INTO t VALUES ('A'), ('B'), ('C')")
        .unwrap();
    let r = eng
        .execute("SELECT CASE kind WHEN 'A' THEN 1 WHEN 'B' THEN 2 ELSE 0 END FROM t")
        .unwrap();
    let rows = match r {
        spg_engine::QueryResult::Rows { rows, .. } => rows,
        _ => panic!("expected rows"),
    };
    let ints: Vec<i64> = rows
        .iter()
        .map(|r| match r.values[0] {
            spg_storage::Value::Int(n) => i64::from(n),
            spg_storage::Value::BigInt(n) => n,
            ref other => panic!("expected int, got {other:?}"),
        })
        .collect();
    assert_eq!(ints, vec![1, 2, 0]);
}
