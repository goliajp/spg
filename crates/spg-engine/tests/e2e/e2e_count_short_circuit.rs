//! v7.35.2 — `COUNT(*) / COUNT(<literal>) / COUNT(<NOT NULL col>)`
//! short-circuit differential. All three shapes must return the
//! same answer (the post-WHERE survivor count) on the same input,
//! and a nullable-column `COUNT(<col>)` must still take the slow
//! path so its NULL-skipping semantics are honoured.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn one_count(eng: &mut Engine, sql: &str) -> i64 {
    match eng.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            Value::BigInt(n) => n,
            ref other => panic!("expected BigInt, got {other:?} for {sql}"),
        },
        other => panic!("expected Rows, got {other:?} for {sql}"),
    }
}

#[test]
fn count_literal_and_not_null_col_match_count_star() {
    let mut eng = Engine::new();
    eng.execute(
        "CREATE TABLE t (id BIGINT NOT NULL PRIMARY KEY, payload TEXT NOT NULL, optional_note TEXT)",
    )
    .unwrap();
    for i in 1..=10 {
        let note = if i % 3 == 0 { "NULL" } else { "'note'" };
        eng.execute(&format!(
            "INSERT INTO t (id, payload, optional_note) VALUES ({i}, 'p-{i}', {note})"
        ))
        .unwrap();
    }
    // Bare table.
    let cs = one_count(&mut eng, "SELECT COUNT(*) FROM t");
    let c1 = one_count(&mut eng, "SELECT COUNT(1) FROM t");
    let c_id = one_count(&mut eng, "SELECT COUNT(id) FROM t");
    let c_payload = one_count(&mut eng, "SELECT COUNT(payload) FROM t");
    assert_eq!(cs, 10);
    assert_eq!(c1, 10);
    assert_eq!(c_id, 10);
    assert_eq!(c_payload, 10);

    // Nullable column — COUNT(col) must skip NULLs, so it differs.
    let c_optional = one_count(&mut eng, "SELECT COUNT(optional_note) FROM t");
    // 10 rows, every 3rd (3,6,9) is NULL → 7 non-NULL.
    assert_eq!(c_optional, 7);

    // With a WHERE filter — the short-circuit takes the post-WHERE
    // survivor count, so the three NOT-NULL shapes match each other.
    let f_cs = one_count(&mut eng, "SELECT COUNT(*) FROM t WHERE id > 5");
    let f_c1 = one_count(&mut eng, "SELECT COUNT(1) FROM t WHERE id > 5");
    let f_c_id = one_count(&mut eng, "SELECT COUNT(id) FROM t WHERE id > 5");
    assert_eq!(f_cs, 5);
    assert_eq!(f_c1, 5);
    assert_eq!(f_c_id, 5);
}

/// Guard against the qualifier branch: when the column reference is
/// qualified (`t.id`) and matches the table alias, the short-circuit
/// must still take effect.
#[test]
fn count_qualified_not_null_col_matches_count_star() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id BIGINT NOT NULL PRIMARY KEY)")
        .unwrap();
    for i in 1..=4 {
        eng.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
    }
    assert_eq!(one_count(&mut eng, "SELECT COUNT(t.id) FROM t"), 4);
    assert_eq!(
        one_count(&mut eng, "SELECT COUNT(t.id) FROM t WHERE id < 3"),
        2
    );
}
