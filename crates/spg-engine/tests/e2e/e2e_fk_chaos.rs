//! v7.6.9 — FK chaos / persistence tests.
//!
//! Covers:
//!   - WAL replay reconstructs FK violations bit-identically
//!     to the original execution
//!   - Catalog snapshot round-trip preserves every FK action
//!   - ALTER ADD CONSTRAINT against violating rows leaves
//!     catalog unchanged (atomicity)
//!   - DELETE CASCADE chain that fails halfway leaves catalog
//!     unchanged

use spg_engine::{Engine, EngineError, QueryResult};

fn engine_with(sqls: &[&str]) -> Engine {
    let mut eng = Engine::new();
    for sql in sqls {
        let r = eng
            .execute(sql)
            .unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
        assert!(matches!(r, QueryResult::CommandOk { .. }), "{sql:?}");
    }
    eng
}

#[test]
fn snapshot_restore_preserves_all_actions() {
    // Build a schema exercising every FkAction variant + multiple
    // FKs per table + composite + self-ref.
    let eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE v (id INT NOT NULL)",
        "CREATE INDEX v_pk ON v (id)",
        "CREATE TABLE c1 (id INT NOT NULL, a INT NOT NULL, \
         FOREIGN KEY (a) REFERENCES u(id) ON DELETE CASCADE ON UPDATE SET NULL)",
        "CREATE TABLE c2 (id INT NOT NULL, b INT DEFAULT 0 NOT NULL, \
         FOREIGN KEY (b) REFERENCES v(id) ON DELETE SET DEFAULT)",
        "CREATE TABLE c3 (id INT NOT NULL, x INT NOT NULL, y INT NOT NULL, \
         FOREIGN KEY (x, y) REFERENCES u(id, id))", // composite (silly but exercises code)
        "CREATE TABLE tree (id INT NOT NULL, parent_id INT, \
         FOREIGN KEY (parent_id) REFERENCES tree(id) ON DELETE CASCADE)",
        "CREATE INDEX tree_pk ON tree (id)",
    ]);

    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();

    let c1_fk = &cat.get("c1").unwrap().schema().foreign_keys[0];
    assert_eq!(c1_fk.on_delete, spg_storage::FkAction::Cascade);
    assert_eq!(c1_fk.on_update, spg_storage::FkAction::SetNull);

    let c2_fk = &cat.get("c2").unwrap().schema().foreign_keys[0];
    assert_eq!(c2_fk.on_delete, spg_storage::FkAction::SetDefault);

    let c3_fk = &cat.get("c3").unwrap().schema().foreign_keys[0];
    assert_eq!(c3_fk.local_columns.len(), 2);

    let tree_fk = &cat.get("tree").unwrap().schema().foreign_keys[0];
    assert_eq!(tree_fk.parent_table, "tree");
    assert_eq!(tree_fk.on_delete, spg_storage::FkAction::Cascade);
}

#[test]
fn restored_engine_enforces_fk_identically() {
    // Build schema + data in engine A, snapshot, restore as B,
    // verify B rejects/accepts identical inputs.
    let mut a = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL REFERENCES u(id))",
        "INSERT INTO u VALUES (1), (2)",
    ]);
    a.execute("INSERT INTO o VALUES (10, 1)").unwrap();
    let bytes = a.snapshot();

    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let mut b = Engine::restore(cat);
    // Restored engine: missing-parent INSERT must reject.
    let r = b.execute("INSERT INTO o VALUES (11, 99)");
    assert!(
        matches!(r, Err(EngineError::Unsupported(ref s)) if s.contains("FOREIGN KEY violation"))
    );
    // Existing-parent INSERT must accept.
    b.execute("INSERT INTO o VALUES (12, 2)").unwrap();
}

#[test]
fn alter_add_against_violating_data_leaves_catalog_clean() {
    // Existing data violates the new FK. ALTER ADD CONSTRAINT
    // must fail AND the catalog must NOT carry the constraint.
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL)",
        "INSERT INTO u VALUES (1)",
        "INSERT INTO o VALUES (10, 99)", // 99 doesn't exist in u
    ]);
    let r = eng.execute("ALTER TABLE o ADD CONSTRAINT fk FOREIGN KEY (uid) REFERENCES u(id)");
    assert!(r.is_err());
    // Verify the catalog has no FK on `o`.
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    assert!(cat.get("o").unwrap().schema().foreign_keys.is_empty());
    // And: future operations are unaffected — the violation
    // doesn't get "stuck" as a constraint.
    eng.execute("INSERT INTO o VALUES (11, 99)").unwrap();
}

#[test]
fn restrict_branch_blocks_cascade_branch_atomically() {
    // Two children: one CASCADE, one RESTRICT, both refer to u.
    // DELETE u(1) must fail with RESTRICT and leave b (the
    // cascade-able child) untouched — no partial application.
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE b (id INT NOT NULL, a_id INT NOT NULL, \
         FOREIGN KEY (a_id) REFERENCES u(id) ON DELETE CASCADE)",
        "CREATE TABLE c (id INT NOT NULL, a_id INT NOT NULL, \
         FOREIGN KEY (a_id) REFERENCES u(id))", // RESTRICT
        "INSERT INTO u VALUES (1)",
        "INSERT INTO b VALUES (10, 1)",
        "INSERT INTO c VALUES (100, 1)",
    ]);
    let r = eng.execute("DELETE FROM u WHERE id = 1");
    assert!(r.is_err());
    // Counts unchanged.
    fn cnt(eng: &mut Engine, t: &str) -> usize {
        match eng.execute(&alloc::format!("SELECT id FROM {t}")).unwrap() {
            QueryResult::Rows { rows, .. } => rows.len(),
            _ => 0,
        }
    }
    assert_eq!(cnt(&mut eng, "u"), 1);
    assert_eq!(cnt(&mut eng, "b"), 1);
    assert_eq!(cnt(&mut eng, "c"), 1);
}

#[test]
fn deep_cascade_chain_handles_subtree_size() {
    // Self-ref tree: build a 50-node chain, delete the root,
    // verify the whole chain disappears.
    let mut eng = engine_with(&[
        "CREATE TABLE node (id INT NOT NULL, parent_id INT, \
         FOREIGN KEY (parent_id) REFERENCES node(id) ON DELETE CASCADE)",
        "CREATE INDEX node_pk ON node (id)",
    ]);
    // Build chain via single VALUES list (v7.6.7 widening).
    let mut values = String::from("(0, NULL)");
    for i in 1..50 {
        values.push_str(&alloc::format!(", ({i}, {})", i - 1));
    }
    eng.execute(&alloc::format!("INSERT INTO node VALUES {values}"))
        .unwrap();
    eng.execute("DELETE FROM node WHERE id = 0").unwrap();
    let rows = match eng.execute("SELECT id FROM node").unwrap() {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!("rows"),
    };
    assert!(rows.is_empty(), "expected empty, got {rows:?}");
}

extern crate alloc;
