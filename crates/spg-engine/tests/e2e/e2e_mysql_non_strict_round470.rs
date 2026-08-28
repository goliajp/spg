//! read01 round 470 (C12) — MySQL's non-strict `sql_mode`.
//!
//! SPG tracked `sql_mode` only for its backslash-escape bit and reported a
//! hardcoded strict value from SHOW. So `SET sql_mode=''` was accepted and
//! then ignored: every out-of-range or malformed value still raised, where
//! MariaDB 11 bends it to fit and carries on. A MySQL application that
//! relies on non-strict loading — most legacy ones do — could not run.
//!
//! The conversion itself already existed: round 434 built `mysql_ignore_fit`
//! for `INSERT IGNORE`. Non-strict is the same conversion on a different
//! trigger, per-session instead of per-statement.
//!
//! Measured on MariaDB 11, and the two triggers are NOT identical — the one
//! place they differ is what makes this its own rule rather than an alias:
//!
//!   case                                   non-strict   IGNORE
//!   explicit NULL into NOT NULL            ERROR 1048   stores 0
//!   omitted NOT NULL column, no DEFAULT    stores 0     stores 0
//!
//! and in a strict session the omitted column raises 1364, a different
//! errno from the 1048 an explicit NULL gets.

use spg_engine::{Engine, QueryResult};

fn my() -> Engine {
    let mut e = Engine::new();
    e.set_mysql_wire_session();
    e
}

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql} -> {other:?}"),
    }
}

#[test]
fn round470_non_strict_bends_values_to_fit() {
    let mut e = my();
    e.execute("SET sql_mode=''").unwrap();
    e.execute("CREATE TABLE t (id INT, i INT, ti TINYINT, u INT UNSIGNED, v VARCHAR(3))")
        .unwrap();
    // Every stored value below is what MariaDB 11 stores with sql_mode=''.
    for sql in [
        "INSERT INTO t (id,i) VALUES (1, 99999999999999)",
        "INSERT INTO t (id,ti) VALUES (2, 999)",
        "INSERT INTO t (id,u) VALUES (3, -5)",
        "INSERT INTO t (id,v) VALUES (4, 'toolong')",
        "INSERT INTO t (id,i) VALUES (5, 'abc')",
        "INSERT INTO t (id,i) VALUES (6, '12xy')",
    ] {
        e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
    }
    assert_eq!(
        rows(&mut e, "SELECT id,i,ti,u,v FROM t ORDER BY id"),
        vec![
            "1|2147483647|NULL|NULL|NULL".to_string(),
            "2|NULL|127|NULL|NULL".to_string(),
            "3|NULL|NULL|0|NULL".to_string(),
            "4|NULL|NULL|NULL|too".to_string(),
            "5|0|NULL|NULL|NULL".to_string(),
            "6|12|NULL|NULL|NULL".to_string(),
        ]
    );
}

#[test]
fn round470_strict_still_raises() {
    // The default is strict, and the same statements must still be refused.
    let mut e = my();
    e.execute("CREATE TABLE t (id INT, ti TINYINT, v VARCHAR(3))")
        .unwrap();
    assert!(e.execute("INSERT INTO t (id,ti) VALUES (1, 999)").is_err());
    assert!(
        e.execute("INSERT INTO t (id,v) VALUES (2, 'toolong')")
            .is_err()
    );
    // And turning strictness back on mid-session takes effect.
    e.execute("SET sql_mode=''").unwrap();
    e.execute("INSERT INTO t (id,ti) VALUES (3, 999)").unwrap();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    assert!(e.execute("INSERT INTO t (id,ti) VALUES (4, 999)").is_err());
}

#[test]
fn round470_null_into_not_null_follows_the_measured_four_way_rule() {
    let mut e = my();
    e.execute("CREATE TABLE t3 (id INT, n INT NOT NULL, s VARCHAR(3) NOT NULL)")
        .unwrap();

    e.execute("SET sql_mode=''").unwrap();
    // Non-strict does NOT bend an explicit NULL — MariaDB still raises 1048.
    assert!(
        e.execute("INSERT INTO t3 (id,n,s) VALUES (1, NULL, 'ab')")
            .is_err(),
        "an explicit NULL into NOT NULL must still be refused in a non-strict session"
    );
    // An omitted column takes the type's implicit default.
    e.execute("INSERT INTO t3 (id) VALUES (2)").unwrap();
    // IGNORE bends the explicit NULL, in either mode.
    e.execute("INSERT IGNORE INTO t3 (id,n,s) VALUES (3, NULL, 'ab')")
        .unwrap();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e.execute("INSERT IGNORE INTO t3 (id,n,s) VALUES (4, NULL, 'ab')")
        .unwrap();
    // A strict session refuses the omitted column with MySQL's own wording.
    let err = e
        .execute("INSERT INTO t3 (id) VALUES (5)")
        .expect_err("strict must refuse an omitted NOT NULL column");
    assert!(
        format!("{err}").contains("Field 'n' doesn't have a default value"),
        "got: {err}"
    );

    assert_eq!(
        rows(&mut e, "SELECT id,n,s FROM t3 ORDER BY id"),
        vec![
            "2|0|".to_string(),
            "3|0|ab".to_string(),
            "4|0|ab".to_string(),
        ]
    );
}

#[test]
fn round470_postgres_sessions_are_untouched() {
    // `sql_mode` is a MySQL surface; a PG session keeps raising.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT, v VARCHAR(3))").unwrap();
    assert!(
        e.execute("INSERT INTO t VALUES (1, 'toolong')").is_err(),
        "PG never truncates on overflow, whatever sql_mode says"
    );
}
