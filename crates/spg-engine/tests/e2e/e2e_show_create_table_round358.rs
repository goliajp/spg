//! read01 round 358 (MySQL differential, M16) — SHOW CREATE TABLE.
//!
//! mysqldump round-trips a schema through exactly this statement, and the
//! output was missing more than formatting:
//!
//!   * every column DEFAULT was dropped — `b BIGINT NOT NULL DEFAULT 7`
//!     came back with no default at all;
//!   * every secondary index was dropped;
//!   * AUTO_INCREMENT was dropped from the identity column, and the
//!     table's own `AUTO_INCREMENT=n` option with it.
//!
//! A client dumping and reloading lost its defaults and its indexes
//! without a word. The rest is fidelity: MariaDB 11 prints types in
//! lower case with a display width, `DEFAULT NULL` for a nullable column
//! that has none, `current_timestamp()` for the clock default, and its
//! keys in the order PRIMARY, UNIQUE, KEY.
//!
//! The expected text below is the MariaDB 11 output for this schema,
//! with the two differences SPG cannot honestly produce noted in the
//! test itself.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn ddl(e: &mut Engine, table: &str) -> String {
    match e
        .execute(&format!("SHOW CREATE TABLE {table}"))
        .unwrap_or_else(|err| panic!("{err}"))
    {
        QueryResult::Rows { rows, .. } => match rows.first().and_then(|r| r.values.get(1)) {
            Some(Value::Text(t)) => t.to_string(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new().with_clock(|| 1_784_723_696_541_528);
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e.execute(
        "CREATE TABLE sc (
           id INT AUTO_INCREMENT PRIMARY KEY,
           n INT,
           b BIGINT NOT NULL DEFAULT 7,
           t TEXT,
           v VARCHAR(20) NOT NULL,
           d DECIMAL(10,2) DEFAULT NULL,
           ts DATETIME DEFAULT CURRENT_TIMESTAMP,
           f DOUBLE)",
    )
    .unwrap();
    e.execute("CREATE INDEX idx_n ON sc (n)").unwrap();
    e.execute("CREATE UNIQUE INDEX uq_v ON sc (v)").unwrap();
    e.execute("INSERT INTO sc (n,b,v) VALUES (1,2,'a'),(3,4,'b')")
        .unwrap();
    e
}

/// The whole statement, as MariaDB renders it.
#[test]
fn it_renders_mariadbs_shape() {
    let mut e = fixture();
    assert_eq!(
        ddl(&mut e, "sc"),
        "CREATE TABLE `sc` (\n  \
         `id` int(11) NOT NULL AUTO_INCREMENT,\n  \
         `n` int(11) DEFAULT NULL,\n  \
         `b` bigint(20) NOT NULL DEFAULT 7,\n  \
         `t` text DEFAULT NULL,\n  \
         `v` varchar(20) NOT NULL,\n  \
         `d` decimal(10,2) DEFAULT NULL,\n  \
         `ts` datetime DEFAULT current_timestamp(),\n  \
         `f` double DEFAULT NULL,\n  \
         PRIMARY KEY (`id`),\n  \
         UNIQUE KEY `uq_v` (`v`),\n  \
         KEY `idx_n` (`n`)\n\
         ) ENGINE=InnoDB AUTO_INCREMENT=3 DEFAULT CHARSET=utf8mb4",
    );
}

/// The three things that were being lost, called out one by one so a
/// regression names itself.
#[test]
fn nothing_is_dropped_any_more() {
    let mut e = fixture();
    let out = ddl(&mut e, "sc");
    assert!(
        out.contains("NOT NULL DEFAULT 7"),
        "a column default: {out}"
    );
    assert!(
        out.contains("KEY `idx_n` (`n`)"),
        "a secondary index: {out}"
    );
    assert!(
        out.contains("UNIQUE KEY `uq_v` (`v`)"),
        "a unique index, as UNIQUE: {out}"
    );
    assert!(out.contains("AUTO_INCREMENT,"), "the column marker: {out}");
    assert!(
        out.contains("AUTO_INCREMENT=3"),
        "the table's next value: {out}"
    );
}

/// The auto-increment option tracks the table, rather than being a
/// constant that happens to match.
#[test]
fn the_auto_increment_option_is_the_real_next_value() {
    let mut e = fixture();
    assert!(ddl(&mut e, "sc").contains("AUTO_INCREMENT=3"));
    e.execute("INSERT INTO sc (n,b,v) VALUES (9,9,'c')")
        .unwrap();
    assert!(ddl(&mut e, "sc").contains("AUTO_INCREMENT=4"));
}

/// A table with no auto-increment column carries no such option, and a
/// NOT NULL column with no default gets no `DEFAULT NULL`.
#[test]
fn the_shape_without_an_identity_column() {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e.execute("CREATE TABLE p (a INT NOT NULL, b TEXT)")
        .unwrap();
    let out = ddl(&mut e, "p");
    assert!(!out.contains("AUTO_INCREMENT"), "{out}");
    assert!(out.contains("`a` int(11) NOT NULL,"), "{out}");
    assert!(out.contains("`b` text DEFAULT NULL"), "{out}");
}
