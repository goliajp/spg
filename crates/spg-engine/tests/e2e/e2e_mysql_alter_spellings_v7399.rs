//! v7.39.9 — the ALTER TABLE vocabulary a MySQL migration writes.
//!
//! A sweep of thirty-seven MySQL spellings against MySQL 9.7.2 beside
//! the published 7.39.8 image found eleven that MySQL accepts and SPG
//! answered `1064 syntax error` for. These are the ALTER family among
//! them, and each contract below is measured rather than assumed:
//!
//! ```text
//!   MODIFY COLUMN b BIGINT          b becomes bigint, NULLABLE, no default
//!   MODIFY … BIGINT NOT NULL DEF 7  restating them keeps them
//!   CHANGE COLUMN b bb BIGINT       the same, and renames
//!   ADD COLUMN d INT AFTER a        lands at ordinal 3, pushes b to 4
//!   ADD COLUMN e INT FIRST          lands at ordinal 1
//!   MODIFY COLUMN b INT AFTER id    moves an existing column
//!   AUTO_INCREMENT = 100            the next insert takes 100
//!   ENGINE = InnoDB                 accepted; NONSUCH is 1286
//!   CONVERT TO CHARACTER SET utf8mb4  accepted; a bogus one is 1115
//!   RENAME INDEX kk TO kk2          renames; a missing key is 1176
//! ```
//!
//! `MODIFY` and `CHANGE` REPLACE the definition, which is the part no
//! PostgreSQL spelling expresses — a version that only changed the type
//! would silently keep a NOT NULL the migration asked to lift. `AFTER`
//! and `FIRST` really move the column: the row encoding is positional
//! and `SELECT *` reads it in order, so appending instead would be a
//! wrong answer rather than a missing feature.

use spg_engine::{Engine, QueryResult};

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.set_mysql_dialect(true);
    e.execute(
        "CREATE TABLE m1 (id INT PRIMARY KEY AUTO_INCREMENT, \
         a VARCHAR(64) NOT NULL DEFAULT 'd', b INT NOT NULL DEFAULT 5)",
    )
    .unwrap();
    e.execute("INSERT INTO m1 (a, b) VALUES ('x', 1)").unwrap();
    e
}

/// `(ordinal, name, nullable, default)` for every column, in order.
fn columns(e: &mut Engine) -> Vec<(String, bool, Option<String>)> {
    let QueryResult::Rows { rows, .. } = e
        .execute(
            "SELECT column_name, is_nullable, column_default \
             FROM information_schema.columns WHERE table_name = 'm1' \
             ORDER BY ordinal_position",
        )
        .unwrap()
    else {
        panic!("expected Rows");
    };
    rows.iter()
        .map(|r| {
            let name = match &r.values[0] {
                spg_storage::Value::Text(t) => t.to_string(),
                o => panic!("{o:?}"),
            };
            let nullable =
                matches!(&r.values[1], spg_storage::Value::Text(t) if t.as_ref() == "YES");
            let default = match &r.values[2] {
                spg_storage::Value::Text(t) => Some(t.to_string()),
                spg_storage::Value::Null => None,
                o => panic!("{o:?}"),
            };
            (name, nullable, default)
        })
        .collect()
}

fn names(e: &mut Engine) -> Vec<String> {
    columns(e).into_iter().map(|(n, _, _)| n).collect()
}

#[test]
fn modify_replaces_the_definition() {
    let mut e = mysql();
    e.execute("ALTER TABLE m1 MODIFY COLUMN b BIGINT").unwrap();
    let c = columns(&mut e);
    let b = c.iter().find(|(n, _, _)| n == "b").expect("b");
    assert!(b.1, "MySQL's MODIFY drops NOT NULL when it is not restated");
    assert_eq!(
        b.2, None,
        "MySQL's MODIFY drops the DEFAULT when it is not restated"
    );
}

#[test]
fn modify_keeps_what_it_restates() {
    let mut e = mysql();
    e.execute("ALTER TABLE m1 MODIFY COLUMN b BIGINT NOT NULL DEFAULT 7")
        .unwrap();
    let c = columns(&mut e);
    let b = c.iter().find(|(n, _, _)| n == "b").expect("b");
    assert!(!b.1);
    assert_eq!(b.2.as_deref(), Some("7"));
}

#[test]
fn change_renames_and_replaces() {
    let mut e = mysql();
    e.execute("ALTER TABLE m1 CHANGE COLUMN b bb BIGINT")
        .unwrap();
    let c = columns(&mut e);
    assert!(c.iter().all(|(n, _, _)| n != "b"), "the old name survived");
    let bb = c.iter().find(|(n, _, _)| n == "bb").expect("bb");
    assert!(bb.1);
    assert_eq!(bb.2, None);
}

#[test]
fn after_and_first_put_the_column_where_they_say() {
    let mut e = mysql();
    e.execute("ALTER TABLE m1 ADD COLUMN d INT AFTER a")
        .unwrap();
    assert_eq!(names(&mut e), vec!["id", "a", "d", "b"]);
    e.execute("ALTER TABLE m1 ADD COLUMN f INT FIRST").unwrap();
    assert_eq!(names(&mut e), vec!["f", "id", "a", "d", "b"]);
}

#[test]
fn a_moved_column_carries_its_values() {
    // The row encoding is positional: if the cells did not move with
    // the column, every row would read the wrong value.
    let mut e = mysql();
    e.execute("ALTER TABLE m1 MODIFY COLUMN b INT AFTER id")
        .unwrap();
    assert_eq!(names(&mut e), vec!["id", "b", "a"]);
    let QueryResult::Rows { rows, .. } = e.execute("SELECT b, a FROM m1").unwrap() else {
        panic!("expected Rows");
    };
    assert_eq!(rows.len(), 1);
    assert!(
        matches!(&rows[0].values[0], spg_storage::Value::Int(1)),
        "b reads {:?}, not the 1 it held",
        rows[0].values[0]
    );
    assert!(
        matches!(&rows[0].values[1], spg_storage::Value::Text(t) if t.as_ref() == "x"),
        "a reads {:?}, not the 'x' it held",
        rows[0].values[1]
    );
}

#[test]
fn auto_increment_sets_the_next_value() {
    let mut e = mysql();
    e.execute("ALTER TABLE m1 AUTO_INCREMENT = 100").unwrap();
    e.execute("INSERT INTO m1 (a, b) VALUES ('y', 2)").unwrap();
    let QueryResult::Rows { rows, .. } = e.execute("SELECT id FROM m1 ORDER BY id").unwrap() else {
        panic!("expected Rows");
    };
    let ids: Vec<i64> = rows
        .iter()
        .map(|r| match &r.values[0] {
            spg_storage::Value::Int(n) => i64::from(*n),
            spg_storage::Value::BigInt(n) => *n,
            o => panic!("{o:?}"),
        })
        .collect();
    assert_eq!(ids, vec![1, 100]);
}

#[test]
fn a_storage_engine_mysql_does_not_know_is_refused() {
    let mut e = mysql();
    e.execute("ALTER TABLE m1 ENGINE = InnoDB")
        .expect("a name MySQL knows");
    let err = e.execute("ALTER TABLE m1 ENGINE = NONSUCH").unwrap_err();
    assert!(
        format!("{err:?}").contains("Unknown storage engine"),
        "a typo must not quietly become SPG's storage: {err:?}"
    );
}

#[test]
fn a_charset_spg_cannot_represent_is_refused() {
    let mut e = mysql();
    e.execute("ALTER TABLE m1 CONVERT TO CHARACTER SET utf8mb4")
        .expect("utf8mb4 is what SPG stores");
    let err = e
        .execute("ALTER TABLE m1 CONVERT TO CHARACTER SET nosuchcs")
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("Unknown character set"),
        "{err:?}"
    );
}

#[test]
fn rename_index_renames_and_says_so_when_it_cannot() {
    let mut e = mysql();
    e.execute("CREATE INDEX kk ON m1 (a)").unwrap();
    e.execute("ALTER TABLE m1 RENAME INDEX kk TO kk2").unwrap();
    let QueryResult::Rows { rows, .. } = e
        .execute("SELECT indexname FROM pg_indexes WHERE tablename = 'm1'")
        .unwrap()
    else {
        panic!("expected Rows");
    };
    let found: Vec<String> = rows
        .iter()
        .map(|r| match &r.values[0] {
            spg_storage::Value::Text(t) => t.to_string(),
            o => panic!("{o:?}"),
        })
        .collect();
    assert!(found.iter().any(|n| n == "kk2"), "{found:?}");
    assert!(!found.iter().any(|n| n == "kk"), "{found:?}");

    // A key that is not there is MySQL's 1176, a DIFFERENT failure from
    // the 1091 `DROP INDEX` answers — so the sentence must differ too.
    let err = e
        .execute("ALTER TABLE m1 RENAME INDEX k_none TO k_new")
        .unwrap_err();
    let text = format!("{err:?}");
    assert!(
        text.contains("Key 'k_none' doesn't exist in table"),
        "{text}"
    );
}

#[test]
fn rename_table_takes_several_pairs() {
    let mut e = mysql();
    e.execute("CREATE TABLE m2 (z INT)").unwrap();
    e.execute("RENAME TABLE m1 TO m1x, m2 TO m2x").unwrap();
    let QueryResult::Rows { rows, .. } = e
        .execute("SELECT tablename FROM pg_tables WHERE tablename LIKE 'm%' ORDER BY tablename")
        .unwrap()
    else {
        panic!("expected Rows");
    };
    let found: Vec<String> = rows
        .iter()
        .map(|r| match &r.values[0] {
            spg_storage::Value::Text(t) => t.to_string(),
            o => panic!("{o:?}"),
        })
        .collect();
    assert_eq!(found, vec!["m1x", "m2x"]);
}

#[test]
fn analyze_table_answers_with_mysqls_result_set() {
    // MySQL returns rows here, not a command tag: measured,
    // `bench.m1  analyze  status  OK`. A client reading those rows gets
    // nothing from a tag.
    let mut e = mysql();
    let QueryResult::Rows { columns, rows } = e.execute("ANALYZE TABLE m1").unwrap() else {
        panic!("ANALYZE TABLE must answer with rows on the MySQL dialect");
    };
    assert_eq!(
        columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        vec!["Table", "Op", "Msg_type", "Msg_text"]
    );
    assert_eq!(rows.len(), 1);
    let cells: Vec<String> = rows[0]
        .values
        .iter()
        .map(|v| match v {
            spg_storage::Value::Text(t) => t.to_string(),
            o => panic!("{o:?}"),
        })
        .collect();
    assert!(cells[0].ends_with("m1"), "{cells:?}");
    assert_eq!(&cells[1..], &["analyze", "status", "OK"]);
}

#[test]
fn straight_join_is_a_hint_not_a_column() {
    // Measured: `SELECT STRAIGHT_JOIN a FROM t` returns the rows on
    // MySQL 9.7.2 and answered `Unknown column 'straight_join'` here.
    let mut e = mysql();
    let QueryResult::Rows { rows, .. } = e.execute("SELECT STRAIGHT_JOIN a FROM m1").unwrap()
    else {
        panic!("expected Rows");
    };
    assert_eq!(rows.len(), 1);
    assert!(matches!(&rows[0].values[0], spg_storage::Value::Text(t) if t.as_ref() == "x"));
}

#[test]
fn the_postgresql_dialect_does_not_take_the_hint() {
    // PostgreSQL has no STRAIGHT_JOIN, and there it really is a column
    // name — accepting it would answer a different query.
    let mut e = Engine::new();
    e.execute("CREATE TABLE p (straight_join INT)").unwrap();
    e.execute("INSERT INTO p VALUES (7)").unwrap();
    let QueryResult::Rows { rows, .. } = e.execute("SELECT straight_join FROM p").unwrap() else {
        panic!("expected Rows");
    };
    assert!(matches!(&rows[0].values[0], spg_storage::Value::Int(7)));
}
