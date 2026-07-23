//! read01 round 386 (MySQL type-fidelity epic — P1) — a TINYINT /
//! MEDIUMINT column records its declared narrow width so a later stage can
//! enforce the range the storage type (SmallInt / Int) is too wide to hold.
//!
//! P1 is pure plumbing: the parser captures the width before the type
//! collapses, the engine copies it to `ColumnSchema.mysql_int_width`, and
//! it survives a catalog snapshot round-trip (FILE_VERSION 81). No behavior
//! change yet — the write-path range check is P2. `SMALLINT` / `INT` /
//! `BIGINT` need no marker (their storage type is faithful), `TINYINT(1)`
//! is Bool, and a PostgreSQL session records nothing.

use spg_engine::Engine;
use spg_storage::MysqlIntWidth;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn width_of(e: &Engine, table: &str, col: &str) -> Option<MysqlIntWidth> {
    let t = e.catalog().get(table).expect("table");
    t.schema()
        .columns
        .iter()
        .find(|c| c.name == col)
        .expect("column")
        .mysql_int_width
}

/// TINYINT -> Tiny, MEDIUMINT -> Medium; SMALLINT / INT / BIGINT -> None.
#[test]
fn narrow_widths_are_recorded() {
    let mut e = mysql();
    e.execute("CREATE TABLE t(a TINYINT, b MEDIUMINT, c SMALLINT, d INT, e BIGINT)")
        .unwrap();
    assert_eq!(width_of(&e, "t", "a"), Some(MysqlIntWidth::Tiny));
    assert_eq!(width_of(&e, "t", "b"), Some(MysqlIntWidth::Medium));
    assert_eq!(width_of(&e, "t", "c"), None);
    assert_eq!(width_of(&e, "t", "d"), None);
    assert_eq!(width_of(&e, "t", "e"), None);
}

/// TINYINT(1) is Bool — not a narrow integer.
#[test]
fn tinyint_one_is_not_a_narrow_int() {
    let mut e = mysql();
    e.execute("CREATE TABLE t(flag TINYINT(1))").unwrap();
    assert_eq!(width_of(&e, "t", "flag"), None);
}

/// A PostgreSQL session records nothing (the width is a MySQL concept).
#[test]
fn postgres_records_no_width() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(a TINYINT, b MEDIUMINT)").unwrap();
    assert_eq!(width_of(&e, "t", "a"), None);
    assert_eq!(width_of(&e, "t", "b"), None);
}

/// The annotation survives a catalog snapshot round-trip (FILE_VERSION 81).
#[test]
fn width_survives_snapshot() {
    let mut e = mysql();
    e.execute("CREATE TABLE t(a TINYINT, b MEDIUMINT, c INT)")
        .unwrap();
    let snapshot = e.catalog().serialize();
    let reloaded = spg_storage::Catalog::deserialize(&snapshot).expect("roundtrip");
    let e2 = Engine::restore(reloaded);
    assert_eq!(width_of(&e2, "t", "a"), Some(MysqlIntWidth::Tiny));
    assert_eq!(width_of(&e2, "t", "b"), Some(MysqlIntWidth::Medium));
    assert_eq!(width_of(&e2, "t", "c"), None);
}
