//! v7.37.17 (17.6 siblings) — information_schema.check_constraints.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.into_iter()
        .map(|row| row.values.into_iter().collect())
        .collect()
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn check_constraints_listed_with_clause() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE cc (age INT CHECK (age >= 0), name TEXT)")
        .unwrap();
    let got = rows(
        &mut e,
        "SELECT constraint_name, check_clause \
         FROM information_schema.check_constraints",
    );
    assert_eq!(got.len(), 1);
    // PG-canonical single-column CHECK name (live PG18.4: cc_age_check),
    // not the old synthetic cc_check0.
    assert_eq!(text(&got[0][0]), "cc_age_check");
    let clause = text(&got[0][1]);
    assert!(
        clause.contains("age") && clause.contains(">="),
        "clause: {clause}"
    );
}

#[test]
fn check_constraints_empty_without_checks() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE nc (v INT)").unwrap();
    let got = rows(&mut e, "SELECT * FROM information_schema.check_constraints");
    assert!(got.is_empty());
}

#[test]
fn pg_indexes_marks_pk_index_unique() {
    // pg_indexes.indexdef reports a PK-backing index as CREATE UNIQUE
    // INDEX (live PG18.4), while a plain secondary index stays
    // non-unique. SPG enforces PK uniqueness via the constraint, so the
    // view infers uniqueness by matching the index's columns to a
    // uniqueness constraint.
    let mut e = Engine::new();
    e.execute("CREATE TABLE pk (id INT PRIMARY KEY, email TEXT)")
        .unwrap();
    e.execute("CREATE INDEX idx_pk_email ON pk (email)")
        .unwrap();
    let got = rows(
        &mut e,
        "SELECT indexname, indexdef FROM pg_indexes WHERE tablename = 'pk' ORDER BY indexname",
    );
    // pk_pkey → UNIQUE; idx_pk_email → plain.
    let pkey = got
        .iter()
        .find(|r| text(&r[0]) == "pk_pkey")
        .expect("pk_pkey row");
    assert!(
        text(&pkey[1]).contains("CREATE UNIQUE INDEX"),
        "pkey indexdef: {}",
        text(&pkey[1])
    );
    let sec = got
        .iter()
        .find(|r| text(&r[0]) == "idx_pk_email")
        .expect("email idx row");
    assert!(
        text(&sec[1]).starts_with("CREATE INDEX"),
        "secondary indexdef: {}",
        text(&sec[1])
    );
}
