//! v7.39.10 — `SHOW INDEX` answers MySQL's row, and the primary key is
//! told apart.
//!
//! Measured against MySQL 9.7.2 on the published 7.39.9 image, same
//! table, same client:
//!
//! ```text
//!                              MySQL 9.7.2      spg 7.39.9
//!   Key_name of the PK           PRIMARY          f1_pkey
//!   Non_unique of the PK           0                1
//!   columns returned              15                7
//! ```
//!
//! `Non_unique = 1` on a PRIMARY KEY is a wrong VALUE, not a spelling —
//! a tool reading it concludes the key is not unique. It came from
//! `!idx.is_unique`, and SPG does not carry the primary key's
//! uniqueness on the index: it lives in the table's uniqueness
//! constraints.
//!
//! MySQL names every primary key `PRIMARY`, so a migration tool looking
//! for that name found nothing. And `SHOW INDEX` has a fixed
//! fifteen-column shape that clients read BY POSITION, so seven columns
//! is not a subset — it is a different result.

use spg_engine::{Engine, QueryResult};

/// MySQL 9.7.2's own column list, in order, copied from a run.
const MYSQL_COLUMNS: &[&str] = &[
    "Table",
    "Non_unique",
    "Key_name",
    "Seq_in_index",
    "Column_name",
    "Collation",
    "Cardinality",
    "Sub_part",
    "Packed",
    "Null",
    "Index_type",
    "Comment",
    "Index_comment",
    "Visible",
    "Expression",
];

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.set_mysql_dialect(true);
    e.execute("CREATE TABLE f1 (id INT PRIMARY KEY, a VARCHAR(64), b INT, c INT)")
        .unwrap();
    e.execute("CREATE INDEX kk ON f1 (a)").unwrap();
    e.execute("CREATE UNIQUE INDEX uu ON f1 (b)").unwrap();
    e
}

fn show(e: &mut Engine) -> (Vec<String>, Vec<Vec<String>>) {
    let QueryResult::Rows { columns, rows } = e.execute("SHOW INDEX FROM f1").unwrap() else {
        panic!("expected Rows");
    };
    let names = columns.iter().map(|c| c.name.clone()).collect();
    let cells = rows
        .iter()
        .map(|r| {
            r.values
                .iter()
                .map(|v| match v {
                    spg_storage::Value::Text(t) => t.to_string(),
                    spg_storage::Value::Int(n) => n.to_string(),
                    spg_storage::Value::BigInt(n) => n.to_string(),
                    spg_storage::Value::Null => "NULL".to_string(),
                    o => panic!("{o:?}"),
                })
                .collect()
        })
        .collect();
    (names, cells)
}

#[test]
fn the_row_has_mysqls_fifteen_columns_in_mysqls_order() {
    let mut e = seeded();
    let (names, _) = show(&mut e);
    assert_eq!(
        names, MYSQL_COLUMNS,
        "a client reading by position gets a different result"
    );
}

#[test]
fn the_primary_key_is_named_primary_and_is_unique() {
    let mut e = seeded();
    let (_, rows) = show(&mut e);
    let pk = rows
        .iter()
        .find(|r| r[4] == "id")
        .expect("a row for the primary key column");
    assert_eq!(pk[2], "PRIMARY", "MySQL names every primary key PRIMARY");
    assert_eq!(
        pk[1], "0",
        "Non_unique = 1 on a PRIMARY KEY tells a client the key is not unique"
    );
}

#[test]
fn the_primary_key_comes_first() {
    let mut e = seeded();
    let (_, rows) = show(&mut e);
    assert_eq!(rows[0][2], "PRIMARY", "{rows:?}");
}

#[test]
fn a_unique_index_is_unique_and_a_plain_one_is_not() {
    let mut e = seeded();
    let (_, rows) = show(&mut e);
    let uu = rows.iter().find(|r| r[2] == "uu").expect("uu");
    let kk = rows.iter().find(|r| r[2] == "kk").expect("kk");
    assert_eq!(uu[1], "0");
    assert_eq!(kk[1], "1");
}

#[test]
fn a_composite_index_gets_one_row_per_column_numbered_from_one() {
    // MySQL emits one row per (index × column); SPG named only the
    // leading one and always said Seq_in_index = 1.
    let mut e = seeded();
    e.execute("CREATE INDEX cc ON f1 (a, c)").unwrap();
    let (_, rows) = show(&mut e);
    let cc: Vec<&Vec<String>> = rows.iter().filter(|r| r[2] == "cc").collect();
    assert_eq!(cc.len(), 2, "one row per column: {rows:?}");
    assert_eq!((cc[0][3].as_str(), cc[0][4].as_str()), ("1", "a"));
    assert_eq!((cc[1][3].as_str(), cc[1][4].as_str()), ("2", "c"));
}

#[test]
fn the_values_spg_cannot_answer_are_mysqls_own() {
    // Copied from a 9.7.2 run on an unanalysed table rather than
    // invented: Collation A, Cardinality 0, Sub_part and Packed NULL,
    // empty comments, Visible YES, Expression NULL.
    let mut e = seeded();
    let (_, rows) = show(&mut e);
    let r = &rows[0];
    assert_eq!(r[5], "A");
    assert_eq!(r[6], "0");
    assert_eq!(r[7], "NULL");
    assert_eq!(r[8], "NULL");
    assert_eq!(r[10], "BTREE");
    assert_eq!(r[11], "");
    assert_eq!(r[12], "");
    assert_eq!(r[13], "YES");
    assert_eq!(r[14], "NULL");
}

#[test]
fn a_nullable_column_says_yes_and_a_not_null_one_says_nothing() {
    let mut e = seeded();
    let (_, rows) = show(&mut e);
    let pk = rows.iter().find(|r| r[4] == "id").expect("id");
    assert_eq!(pk[9], "", "a PRIMARY KEY column is NOT NULL");
    let kk = rows.iter().find(|r| r[2] == "kk").expect("kk");
    assert_eq!(kk[9], "YES");
}

#[test]
fn an_unknown_engine_is_named_back_as_written() {
    // MySQL quotes the operator's own spelling so they can find the
    // typo: measured, `ENGINE=NoSuchEng` answers `Unknown storage
    // engine 'NoSuchEng'`. The lexer folds a bare identifier, so the
    // ALTER path had been quoting `'nosucheng'` — a word the migration
    // does not contain. `CREATE TABLE` has kept the spelling since
    // v7.39.3; this is the same guard on the other statement.
    let mut e = seeded();
    let err = e.execute("ALTER TABLE f1 ENGINE=NoSuchEng").unwrap_err();
    let text = format!("{err:?}");
    assert!(
        text.contains("'NoSuchEng'"),
        "the message must quote the word the migration wrote: {text}"
    );
}
