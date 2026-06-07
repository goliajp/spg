//! v7.13.3 — mailrs round-7 surface coverage.
//! S8: ALTER TABLE … DROP [COLUMN] [IF EXISTS] col [CASCADE/RESTRICT]
//! S9: CREATE TABLE IF NOT EXISTS PG-strict semantics (existing table = no-op).
//!     v7.13.3 originally implemented a "reconcile" path that added
//!     missing columns; v7.16.2 reverts to PG-strict because the
//!     reconcile path silently re-added schema-renamed columns
//!     (mailrs round-10 migrate-040 / migrate-042).
//! S10: '<text>'::jsonb cast produces JSONB (no JSON↔JSONB mismatch)

use spg_engine::Engine;

// ── S8: DROP COLUMN ──────────────────────────────────────────────

#[test]
fn drop_column_basic() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, x INT NOT NULL, y TEXT)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 10, 'a')").unwrap();
    e.execute("ALTER TABLE t DROP COLUMN x").unwrap();
    let table = e.catalog().get("t").unwrap();
    assert_eq!(table.schema().columns.len(), 2);
    assert_eq!(table.schema().columns[0].name, "id");
    assert_eq!(table.schema().columns[1].name, "y");
    // Existing row shrunk to (1, 'a').
    let row = table.rows().get(0).unwrap();
    assert_eq!(row.values.len(), 2);
}

#[test]
fn drop_column_if_exists_idempotent() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, x INT)").unwrap();
    e.execute("ALTER TABLE t DROP COLUMN IF EXISTS x").unwrap();
    // Re-drop is a no-op under IF EXISTS.
    e.execute("ALTER TABLE t DROP COLUMN IF EXISTS x").unwrap();
}

#[test]
fn drop_column_without_if_exists_errors_when_missing() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    let r = e.execute("ALTER TABLE t DROP COLUMN missing");
    assert!(r.is_err());
}

#[test]
fn drop_column_with_fk_dependent_without_cascade_errors() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p (id INT NOT NULL PRIMARY KEY)").unwrap();
    e.execute("CREATE TABLE c (id INT NOT NULL, pid INT REFERENCES p(id))")
        .unwrap();
    let r = e.execute("ALTER TABLE c DROP COLUMN pid");
    assert!(r.is_err(), "expected dependent-FK error, got {r:?}");
}

#[test]
fn drop_column_with_fk_dependent_cascade_drops_fk_too() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p (id INT NOT NULL PRIMARY KEY)").unwrap();
    e.execute("CREATE TABLE c (id INT NOT NULL, pid INT REFERENCES p(id))")
        .unwrap();
    e.execute("ALTER TABLE c DROP COLUMN pid CASCADE").unwrap();
    let c = e.catalog().get("c").unwrap();
    assert_eq!(c.schema().foreign_keys.len(), 0);
}

#[test]
fn drop_column_bare_drop_without_column_keyword() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, x INT)").unwrap();
    e.execute("ALTER TABLE t DROP x").unwrap();
    let table = e.catalog().get("t").unwrap();
    assert_eq!(table.schema().columns.len(), 1);
}

// ── S9: CREATE TABLE IF NOT EXISTS PG-strict semantics ───────────

#[test]
fn create_table_if_not_exists_is_noop_on_existing_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE contacts (id INT NOT NULL, email TEXT)").unwrap();
    // Second create with a different shape — PG-strict semantics:
    // table already exists, so this is a no-op. The "added" columns
    // must NOT appear in the existing table.
    e.execute(
        "CREATE TABLE IF NOT EXISTS contacts (\
           id INT NOT NULL, \
           address_book_id BIGINT, \
           uid TEXT\
         )",
    )
    .unwrap();
    let table = e.catalog().get("contacts").unwrap();
    let names: Vec<&str> = table.schema().columns.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"id"));
    assert!(names.contains(&"email"));
    assert!(!names.contains(&"address_book_id"));
    assert!(!names.contains(&"uid"));
}

#[test]
fn create_table_if_not_exists_does_not_register_inline_fk_on_existing_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE address_books (id BIGINT NOT NULL PRIMARY KEY)")
        .unwrap();
    e.execute("CREATE TABLE contacts (id INT NOT NULL)").unwrap();
    e.execute(
        "CREATE TABLE IF NOT EXISTS contacts (\
           id INT NOT NULL, \
           address_book_id BIGINT NOT NULL REFERENCES address_books(id) ON DELETE CASCADE\
         )",
    )
    .unwrap();
    let contacts = e.catalog().get("contacts").unwrap();
    assert_eq!(contacts.schema().foreign_keys.len(), 0);
}

#[test]
fn create_table_if_not_exists_doesnt_modify_existing_columns() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)").unwrap();
    e.execute("INSERT INTO t VALUES (1, 'alice')").unwrap();
    // Try to "redefine" with a different type for `name`.
    e.execute(
        "CREATE TABLE IF NOT EXISTS t (\
           id INT NOT NULL, \
           name BIGINT, \
           extra TEXT\
         )",
    )
    .unwrap();
    let table = e.catalog().get("t").unwrap();
    assert_eq!(table.schema().columns[1].name, "name");
    assert_eq!(table.schema().columns[1].ty, spg_storage::DataType::Text);
    assert!(!table.schema().columns.iter().any(|c| c.name == "extra"));
}

#[test]
fn create_table_inline_fk_lands_on_column_on_fresh_create() {
    // Round-7 S9's actual requirement: inline REFERENCES in a CREATE
    // TABLE column def must register the column AND the FK. This
    // verifies the inline-FK path on a fresh table (the path exercised
    // by mailrs migrate-023's CREATE TABLE IF NOT EXISTS contacts when
    // `contacts` does NOT yet exist).
    let mut e = Engine::new();
    e.execute("CREATE TABLE address_books (id BIGINT NOT NULL PRIMARY KEY)")
        .unwrap();
    e.execute(
        "CREATE TABLE contacts (\
           id BIGSERIAL PRIMARY KEY, \
           address_book_id BIGINT NOT NULL REFERENCES address_books(id) ON DELETE CASCADE, \
           uid TEXT NOT NULL\
         )",
    )
    .unwrap();
    let contacts = e.catalog().get("contacts").unwrap();
    let names: Vec<&str> = contacts.schema().columns.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"address_book_id"));
    assert_eq!(contacts.schema().foreign_keys.len(), 1);
    assert_eq!(contacts.schema().foreign_keys[0].parent_table, "address_books");
    e.execute("CREATE INDEX idx_book ON contacts(address_book_id)").unwrap();
}

// ── S10: '<text>'::jsonb cast produces JSONB ─────────────────────

#[test]
fn jsonb_cast_default_round_trips() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE jb (id INT NOT NULL)").unwrap();
    e.execute("ALTER TABLE jb ADD COLUMN attendees JSONB NOT NULL DEFAULT '[]'::jsonb")
        .unwrap();
    e.execute("INSERT INTO jb (id) VALUES (1)").unwrap();
    let table = e.catalog().get("jb").unwrap();
    let row = table.rows().get(0).unwrap();
    assert!(matches!(&row.values[1], spg_storage::Value::Json(s) if s == "[]"));
}

#[test]
fn jsonb_cast_object_default() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE jb (id INT NOT NULL)").unwrap();
    e.execute("ALTER TABLE jb ADD COLUMN meta JSONB NOT NULL DEFAULT '{}'::jsonb")
        .unwrap();
    e.execute("ALTER TABLE jb ADD COLUMN config JSONB NOT NULL DEFAULT '{\"version\": 1}'::jsonb")
        .unwrap();
    e.execute("INSERT INTO jb (id) VALUES (1)").unwrap();
    let table = e.catalog().get("jb").unwrap();
    let row = table.rows().get(0).unwrap();
    assert!(matches!(&row.values[1], spg_storage::Value::Json(s) if s == "{}"));
    assert!(matches!(&row.values[2], spg_storage::Value::Json(s) if s.contains("\"version\"")));
}

#[test]
fn jsonb_cast_in_insert_value() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE jb (id INT NOT NULL, data JSONB NOT NULL DEFAULT '[]'::jsonb)")
        .unwrap();
    e.execute("INSERT INTO jb VALUES (1, '[1,2,3]'::jsonb)").unwrap();
    let table = e.catalog().get("jb").unwrap();
    let row = table.rows().get(0).unwrap();
    assert!(matches!(&row.values[1], spg_storage::Value::Json(s) if s == "[1,2,3]"));
}
