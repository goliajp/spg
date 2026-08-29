//! v7.39.2 — `SHOW COLUMNS` / `DESCRIBE` answers MySQL's six columns,
//! and the DDL rendering is MySQL's rather than MariaDB's.
//!
//! `DESCRIBE` is the most-used introspection command on MySQL —
//! SQLAlchemy's mysql dialect reflects with it — and it answers six
//! columns there: Field, Type, Null, Key, Default, Extra. SPG answered
//! three of its own (`name`, `type`, and a raw 0-or-1 `nullable`), so a
//! tool reading `row['Default']` or `row['Extra']` found no such key and
//! could not learn a column's default, its key membership, or that it is
//! AUTO_INCREMENT. The corpus fixture that covered it asserted SPG's own
//! three columns under a header claiming "a live MariaDB 11 run" —
//! MariaDB answers the same six MySQL does.
//!
//! The type spellings were MariaDB's throughout, while SPG advertises
//! itself as MySQL 9.7.2 through one constant that feeds the handshake,
//! `@@version` and `VERSION()`. Measured differences, all now MySQL's:
//!
//!   MySQL 9.7.2          MariaDB 12.3.3        SPG was
//!   int                  int(11)               int(11)
//!   bigint unsigned      bigint(20) unsigned   bigint(20) unsigned
//!   DEFAULT '7'          DEFAULT 7             DEFAULT 7
//!   DEFAULT CURRENT_…    current_timestamp()   current_timestamp()
//!   text                 text DEFAULT NULL     text DEFAULT NULL
//!
//! `tinyint(1)` keeps its width in both: that spelling is how BOOLEAN is
//! written, not a display width.
//!
//! The `AUTO_INCREMENT=n` table option is omitted while the next value
//! is still 1 (measured), where SPG printed `AUTO_INCREMENT=1` on a
//! table that had never been written to — so a freshly created table and
//! its dump did not compare equal.

use spg_engine::{Engine, QueryResult};

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.set_mysql_dialect(true);
    e
}

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect()
            })
            .collect(),
        Ok(other) => panic!("{sql}: {other:?}"),
        Err(err) => panic!("{sql}: {err}"),
    }
}

fn ddl(e: &mut Engine, t: &str) -> String {
    rows(e, &format!("SHOW CREATE TABLE {t}"))[0][1].clone()
}

#[test]
fn describe_answers_mysqls_six_columns() {
    let mut e = mysql();
    e.execute(
        "CREATE TABLE d (id INT AUTO_INCREMENT PRIMARY KEY, u VARCHAR(8) UNIQUE, \
         m INT, s INT DEFAULT 5, t TEXT, ts TIMESTAMP DEFAULT CURRENT_TIMESTAMP)",
    )
    .expect("ddl");
    e.execute("CREATE INDEX idx_m ON d (m)").expect("index");
    let QueryResult::Rows { columns, .. } = e.execute("DESCRIBE d").expect("describe") else {
        panic!("not rows")
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["Field", "Type", "Null", "Key", "Default", "Extra"]);
    // Measured on MySQL 9.7.2 for this schema.
    assert_eq!(
        rows(&mut e, "SHOW COLUMNS FROM d"),
        [
            ["id", "int", "NO", "PRI", "NULL", "auto_increment"],
            ["u", "varchar(8)", "YES", "UNI", "NULL", ""],
            ["m", "int", "YES", "MUL", "NULL", ""],
            ["s", "int", "YES", "", "5", ""],
            ["t", "text", "YES", "", "NULL", ""],
            [
                "ts",
                "datetime",
                "YES",
                "",
                "CURRENT_TIMESTAMP",
                "DEFAULT_GENERATED"
            ],
        ]
    );
    // `DESCRIBE` is the same command under another name.
    assert_eq!(
        rows(&mut e, "DESCRIBE d"),
        rows(&mut e, "SHOW COLUMNS FROM d")
    );
}

#[test]
fn the_ddl_is_mysqls_not_mariadbs() {
    let mut e = mysql();
    e.execute(
        "CREATE TABLE w (id INT AUTO_INCREMENT PRIMARY KEY, b BIGINT NOT NULL DEFAULT 7, \
               u INT UNSIGNED, f BOOLEAN, t TEXT, m BLOB, j JSON)",
    )
    .expect("ddl");
    let out = ddl(&mut e, "w");
    for want in [
        "`id` int NOT NULL AUTO_INCREMENT",
        "`b` bigint NOT NULL DEFAULT '7'",
        "`u` int unsigned DEFAULT NULL",
        // BOOLEAN keeps its width: that IS the spelling, not a display
        // width, and MySQL 9.7.2 prints it.
        "`f` tinyint(1) DEFAULT NULL",
        // TEXT and BLOB cannot carry a literal default, so MySQL writes
        // them bare — JSON, measured, still gets `DEFAULT NULL`.
        "`t` text,",
        "`m` blob,",
        "`j` json DEFAULT NULL",
    ] {
        assert!(out.contains(want), "missing {want}:\n{out}");
    }
    for never in [
        "int(11)",
        "bigint(20)",
        "int(10) unsigned",
        "current_timestamp()",
    ] {
        assert!(!out.contains(never), "MariaDB's spelling {never}:\n{out}");
    }
    // The table option is absent until the table has handed one out.
    assert!(!out.contains("AUTO_INCREMENT="), "{out}");
    e.execute("INSERT INTO w (b) VALUES (1)").expect("insert");
    assert!(
        ddl(&mut e, "w").contains("AUTO_INCREMENT=2"),
        "{}",
        ddl(&mut e, "w")
    );
}

/// The control: a PostgreSQL session is untouched — `SHOW COLUMNS` is
/// not PostgreSQL syntax, so it keeps the shape it had rather than
/// gaining MySQL's.
#[test]
fn a_postgres_session_keeps_its_own_shape() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p (a INT NOT NULL)").expect("ddl");
    let QueryResult::Rows { columns, .. } = e.execute("SHOW COLUMNS FROM p").expect("show") else {
        panic!("not rows")
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["name", "type", "nullable"]);
}
