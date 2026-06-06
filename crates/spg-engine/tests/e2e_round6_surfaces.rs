//! v7.13.2 — mailrs round-6 derived-shape coverage.
//! S1: multi-column ALTER TABLE subactions
//! S2: GIN partial index WHERE
//! S3: inline REFERENCES on ALTER TABLE ADD COLUMN
//! S4: inline REFERENCES in CREATE TABLE col def (verified already works)
//! S5: UNNEST as table function in FROM (cross-join shape + AS p(col))
//! S6: ALTER COLUMN TYPE vector USING NULL (USING-keyword disambiguation)
//! S7: DROP CONSTRAINT IF EXISTS

use spg_engine::Engine;

// ── S1: multi-column ALTER TABLE subactions ──────────────────────

#[test]
fn alter_table_multi_add_column() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    eng.execute(
        "ALTER TABLE t \
            ADD COLUMN IF NOT EXISTS a TEXT, \
            ADD COLUMN IF NOT EXISTS b TIMESTAMPTZ, \
            ADD COLUMN IF NOT EXISTS c TEXT",
    )
    .unwrap();
    let table = eng.catalog().get("t").expect("table");
    assert_eq!(table.schema().columns.len(), 4);
    assert_eq!(table.schema().columns[1].name, "a");
    assert_eq!(table.schema().columns[2].name, "b");
    assert_eq!(table.schema().columns[3].name, "c");
}

#[test]
fn alter_table_mixed_subactions_apply_in_order() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, x TEXT)").unwrap();
    eng.execute("INSERT INTO t VALUES (1, 'old')").unwrap();
    eng.execute(
        "ALTER TABLE t \
            ADD COLUMN y INT DEFAULT 0, \
            ALTER COLUMN x TYPE TEXT",
    )
    .unwrap();
    let table = eng.catalog().get("t").expect("table");
    assert_eq!(table.schema().columns.len(), 3);
}

// ── S2: GIN partial index WHERE ──────────────────────────────────

#[test]
fn gin_index_with_where_predicate_is_accepted() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE docs (id INT NOT NULL, body TEXT)").unwrap();
    eng.execute(
        "CREATE INDEX IF NOT EXISTS idx_body_trgm ON docs \
         USING gin(body gin_trgm_ops) \
         WHERE body IS NOT NULL AND body != ''",
    )
    .unwrap();
}

// ── S3: inline REFERENCES on ALTER TABLE ADD COLUMN ──────────────

#[test]
fn alter_table_add_column_with_inline_references() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE apps (id INT NOT NULL PRIMARY KEY)")
        .unwrap();
    eng.execute("CREATE TABLE api_keys (id INT NOT NULL)").unwrap();
    eng.execute(
        "ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS app_id INT \
         REFERENCES apps(id) ON DELETE CASCADE",
    )
    .unwrap();
    let api_keys = eng.catalog().get("api_keys").expect("table");
    assert_eq!(api_keys.schema().foreign_keys.len(), 1);
    assert_eq!(api_keys.schema().foreign_keys[0].parent_table, "apps");
}

// ── S4: inline REFERENCES in CREATE TABLE (sanity — already worked) ─

#[test]
fn create_table_inline_references_works() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE address_books (id INT NOT NULL PRIMARY KEY)")
        .unwrap();
    eng.execute(
        "CREATE TABLE contacts (\
           id INT NOT NULL PRIMARY KEY, \
           address_book_id INT NOT NULL REFERENCES address_books(id) ON DELETE CASCADE, \
           uid TEXT NOT NULL, \
           UNIQUE(address_book_id, uid)\
         )",
    )
    .unwrap();
    let contacts = eng.catalog().get("contacts").expect("table");
    assert_eq!(contacts.schema().foreign_keys.len(), 1);
}

// ── S5: UNNEST in FROM cross-join ────────────────────────────────

#[test]
fn unnest_cross_join_with_table_and_column_alias() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE groups (id INT NOT NULL, name TEXT, domain TEXT)")
        .unwrap();
    eng.execute(
        "CREATE TABLE group_permissions (\
           group_id INT NOT NULL, \
           permission TEXT NOT NULL, \
           UNIQUE(group_id, permission)\
         )",
    )
    .unwrap();
    eng.execute("INSERT INTO groups VALUES (1, 'super', NULL)").unwrap();
    eng.execute(
        "INSERT INTO group_permissions (group_id, permission) \
         SELECT g.id, p.perm \
         FROM groups g, UNNEST(ARRAY['mail.send','mail.read','admin.domains']) AS p(perm) \
         WHERE g.name = 'super' AND g.domain IS NULL \
         ON CONFLICT DO NOTHING",
    )
    .unwrap();
    let gp = eng.catalog().get("group_permissions").expect("table");
    assert_eq!(gp.rows().len(), 3);
}

#[test]
fn unnest_in_from_primary_position_still_works() {
    let mut eng = Engine::new();
    let r = eng.execute("SELECT * FROM UNNEST(ARRAY['a','b','c'])").unwrap();
    match r {
        spg_engine::QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 3),
        _ => panic!("expected rows"),
    }
}

// ── S6: ALTER COLUMN TYPE vector USING NULL ──────────────────────

#[test]
fn alter_column_type_vector_using_null() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, e VECTOR(768))").unwrap();
    eng.execute(
        "INSERT INTO t VALUES (1, [0.1,0.2,0.3,0.4,0.5,0.6,0.7,0.8,0.9,1.0])",
    )
    .ok(); // dim mismatch ok; we only care about ALTER parsing/eval
    eng.execute("ALTER TABLE t ALTER COLUMN e TYPE VECTOR(1024) USING NULL")
        .unwrap();
}

// ── S7: DROP CONSTRAINT IF EXISTS ────────────────────────────────

#[test]
fn drop_constraint_if_exists_is_idempotent_on_missing() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    // Missing constraint; IF EXISTS makes it a no-op.
    eng.execute("ALTER TABLE t DROP CONSTRAINT IF EXISTS missing_fk")
        .unwrap();
}

#[test]
fn drop_constraint_without_if_exists_errors_on_missing() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    let r = eng.execute("ALTER TABLE t DROP CONSTRAINT missing_fk");
    assert!(r.is_err());
}
