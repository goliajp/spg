//! 7.38.2 S-1 — DROP COLUMN takes its CHECK constraints with it.
//!
//! sentori report 5 (2026-08-18): migration 0012 does `ALTER TABLE
//! push_credentials DROP COLUMN fp` where `fp` carried an inline
//! `CHECK (fp IS NULL OR octet_length(fp) = 32)`. The column left,
//! the CHECK stayed, and every later INSERT died with
//! `ColumnNotFound { name: "fp" }` — the table was permanently
//! un-insertable and the orphan showed in pg_constraint. PG's rule
//! (ALTER TABLE docs): "Indexes and table constraints involving the
//! column will be automatically dropped as well."

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect()
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn pin_v7382_drop_column_drops_inline_check() {
    // The sentori shape, verbatim from their drop-column-check.sql.
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE ck4 (id BIGINT PRIMARY KEY, \
         fp BYTEA CHECK (fp IS NULL OR octet_length(fp) = 32), \
         note TEXT)",
    )
    .unwrap();
    e.execute("INSERT INTO ck4 (id, note) VALUES (1, 'a')")
        .unwrap();
    e.execute("ALTER TABLE ck4 DROP COLUMN fp").unwrap();
    // The whole bug: this INSERT must succeed, not ColumnNotFound{fp}.
    e.execute("INSERT INTO ck4 (id, note) VALUES (2, 'b')")
        .unwrap_or_else(|err| panic!("INSERT after DROP COLUMN: {err}"));
    // And the orphan must be gone from pg_constraint.
    let cons = rows(
        &mut e,
        "SELECT conname FROM pg_constraint WHERE conname LIKE 'ck4%' AND contype = 'c'",
    );
    assert!(
        cons.is_empty(),
        "CHECK survived its column in pg_constraint: {cons:?}"
    );
}

#[test]
fn pin_v7382_drop_column_checks_involving_vs_unrelated() {
    let mut e = Engine::new();
    // c_ab involves the dropped column (multi-column: PG drops it too);
    // c_b does not and must survive with teeth.
    e.execute(
        "CREATE TABLE ck5 (a INT, b INT, \
         CONSTRAINT c_ab CHECK (a < b), \
         CONSTRAINT c_b CHECK (b > 0))",
    )
    .unwrap();
    e.execute("ALTER TABLE ck5 DROP COLUMN a").unwrap();
    e.execute("INSERT INTO ck5 (b) VALUES (5)").unwrap();
    let err = e
        .execute("INSERT INTO ck5 (b) VALUES (-1)")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("c_b"),
        "the unrelated CHECK lost its teeth after a neighbour column drop: {err}"
    );
    let cons = rows(
        &mut e,
        "SELECT conname FROM pg_constraint WHERE conrelid = 'ck5'::regclass \
         AND contype = 'c' ORDER BY conname",
    );
    assert_eq!(
        cons,
        vec![vec!["c_b".to_string()]],
        "exactly the involving CHECK should be gone"
    );
}
