//! v7.17.0 Phase 4.3 — MySQL TINYINT(1) classifies as BOOL.

use spg_engine::{Engine, QueryResult};
use spg_storage::{DataType, Value};

#[test]
fn tinyint_one_creates_bool_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, active TINYINT(1) NOT NULL)")
        .unwrap();
    let table = e.catalog().get("t").unwrap();
    let col = table
        .schema()
        .columns
        .iter()
        .find(|c| c.name == "active")
        .unwrap();
    assert_eq!(col.ty, DataType::Bool);
}

#[test]
fn tinyint_no_width_stays_smallint() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, count TINYINT NOT NULL)")
        .unwrap();
    let table = e.catalog().get("t").unwrap();
    let col = table
        .schema()
        .columns
        .iter()
        .find(|c| c.name == "count")
        .unwrap();
    assert_eq!(col.ty, DataType::SmallInt);
}

#[test]
fn tinyint_explicit_width_other_than_one_stays_smallint() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, level TINYINT(3) NOT NULL)")
        .unwrap();
    let table = e.catalog().get("t").unwrap();
    let col = table
        .schema()
        .columns
        .iter()
        .find(|c| c.name == "level")
        .unwrap();
    assert_eq!(col.ty, DataType::SmallInt);
}

#[test]
fn tinyint_one_accepts_boolean_inserts() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, active TINYINT(1) NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, true), (2, false)")
        .unwrap();
    let r = e.execute("SELECT active FROM t ORDER BY id").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows[0].values[0], Value::Bool(true));
    assert_eq!(rows[1].values[0], Value::Bool(false));
}

// Note: mysqldump emits TINYINT(1) values as `0` / `1` integer
// literals. The integer-to-bool INSERT coercion is a v7.18
// carve-out (engine INSERT-side type coercion is generally
// strict; this would need a broader pass). For v7.17 customers
// can wrap their dump's boolean values as `TRUE` / `FALSE` via
// a `sed` pass, or use the application-side coercion the
// driver already does.

#[test]
fn bool_cast_accepts_pg_prefixes() {
    // PG boolin accepts any unambiguous prefix of true/false/yes/no plus
    // on/off/1/0, case-insensitive and whitespace-trimmed; 'o' alone is
    // ambiguous. All live-PG18.4-verified.
    let mut e = Engine::new();
    let b = |e: &mut Engine, sql: &str| -> bool {
        match e.execute(sql).unwrap() {
            QueryResult::Rows { rows, .. } => matches!(rows[0].values[0], Value::Bool(true)),
            _ => panic!(),
        }
    };
    for t in [
        "t", "tr", "tru", "true", "y", "ye", "yes", "on", "1", "  TRUE ",
    ] {
        assert!(
            b(&mut e, &format!("SELECT '{t}'::bool")),
            "{t} should be true"
        );
    }
    for f in [
        "f", "fa", "fal", "fals", "false", "n", "no", "of", "off", "0",
    ] {
        assert!(
            !b(&mut e, &format!("SELECT '{f}'::bool")),
            "{f} should be false"
        );
    }
    // Ambiguous / garbage rejected.
    assert!(e.execute("SELECT 'o'::bool").is_err(), "'o' is ambiguous");
    assert!(e.execute("SELECT 'maybe'::bool").is_err());
}
