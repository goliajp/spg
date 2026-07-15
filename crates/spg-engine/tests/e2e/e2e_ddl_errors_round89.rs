//! v7.39 (read01 round 89) — a batch of DDL / constraint error messages aligned
//! to PG's exact wording (and, on the wire, SQLSTATE). Each was worded SPG's own
//! way and mostly fell to the generic error class; clients and ORMs branch on
//! these.
//!
//!   * unknown column type: SPG's "column X: unknown column type Y (not a
//!     built-in, ENUM, DOMAIN, or composite)" → PG `type "Y" does not exist`
//!     (42704);
//!   * a column named twice in an INSERT target list came out as a "row arity
//!     mismatch" → PG `column "a" specified more than once` (42701);
//!   * ADD COLUMN NOT NULL with no default on a non-empty table: SPG's own
//!     sentence → PG `column "req" of relation "t" contains null values` (23502);
//!   * DROP INDEX on a missing name: "index not found: x" → PG
//!     `index "x" does not exist` (42704);
//!   * DROP VIEW on a missing name: this one leaked "corrupt on-disk format:"
//!     (a StorageError::Corrupt) → PG `view "x" does not exist` (42P01).

use spg_engine::Engine;

fn err(e: &mut Engine, sql: &str) -> String {
    e.execute(sql).unwrap_err().to_string()
}

#[test]
fn a_unknown_column_type() {
    let mut e = Engine::new();
    assert!(err(&mut e, "CREATE TABLE bad (x notarealtype)")
        .contains("type \"notarealtype\" does not exist"));
}

#[test]
fn b_duplicate_column_in_insert_target() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a int, b int)").unwrap();
    assert!(err(&mut e, "INSERT INTO t(a, a) VALUES (1, 2)")
        .contains("column \"a\" specified more than once"));
}

#[test]
fn c_add_notnull_column_no_default_on_nonempty_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a int)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    assert!(err(&mut e, "ALTER TABLE t ADD COLUMN req int NOT NULL")
        .contains("column \"req\" of relation \"t\" contains null values"));
    // Empty table still accepts it (PG allows).
    let mut e2 = Engine::new();
    e2.execute("CREATE TABLE u (a int)").unwrap();
    e2.execute("ALTER TABLE u ADD COLUMN req int NOT NULL").unwrap();
}

#[test]
fn d_drop_missing_index() {
    let mut e = Engine::new();
    assert!(err(&mut e, "DROP INDEX nosuchindex")
        .contains("index \"nosuchindex\" does not exist"));
}

#[test]
fn e_drop_missing_view_has_no_corrupt_prefix() {
    let mut e = Engine::new();
    let msg = err(&mut e, "DROP VIEW nosuchview");
    assert!(msg.contains("view \"nosuchview\" does not exist"), "got {msg}");
    // The pre-fix message leaked "corrupt on-disk format:" (a Storage::Corrupt).
    assert!(!msg.contains("corrupt"), "leaked Corrupt prefix: {msg}");
}
