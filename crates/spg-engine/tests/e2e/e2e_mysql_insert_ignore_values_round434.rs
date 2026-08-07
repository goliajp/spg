//! read01 round 434 (MySQL differential) — `INSERT IGNORE` also downgrades
//! per-VALUE errors.
//!
//! Round 406 implemented half of MySQL's IGNORE: skip a row that violates a
//! unique key. The other half is that IGNORE turns every per-value error
//! into a coercion, so a bulk load never stops mid-file. SPG raised on all
//! seven shapes measured below, which means a data load that runs on MySQL
//! stopped dead on SPG — a zero-customer-change break, not a cosmetic one.
//!
//! Measured on MariaDB 11:
//!   '12abc' → INT           stores 12
//!   'abc'   → INT           stores 0
//!   'toolong' → VARCHAR(3)  stores 'too'
//!   NULL    → INT NOT NULL  stores 0
//!   99999999999999 → INT    stores 2147483647
//!   999 → TINYINT           stores 127
//!   -5  → TINYINT UNSIGNED  stores 0
//!
//! The string → integer rule is a **float** prefix rounded half away from
//! zero, not a digit scan: '3.7abc' → 4, '2.5' → 3, '-2.5' → -3, '1e3x' →
//! 1000, '.5' → 1, '0x10' → 0, 'abc' / '-' / '' → 0.
//!
//! Two shapes stay loud on purpose. MariaDB stores a `'0000-00-00'` zero
//! date for a bad date and an ENUM's empty error-member for a bad label;
//! SPG can represent neither, so those still raise rather than silently
//! store a value MySQL would not have stored.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn cells(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        spg_storage::Value::Null => "NULL".to_string(),
                        other => spg_engine::eval::value_to_text(other),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn round434_bad_int_string_takes_its_numeric_prefix() {
    let mut e = mysql();
    e.execute("CREATE TABLE t(i INT, n INT NOT NULL)").unwrap();
    e.execute("INSERT IGNORE INTO t(i,n) VALUES ('12abc',1)")
        .unwrap();
    e.execute("INSERT IGNORE INTO t(i,n) VALUES ('abc',2)")
        .unwrap();
    assert_eq!(cells(&mut e, "SELECT i FROM t ORDER BY n"), "12,0");
}

#[test]
fn round434_numeric_prefix_is_a_rounded_float_not_a_digit_scan() {
    let mut e = mysql();
    e.execute("CREATE TABLE v(i INT)").unwrap();
    e.execute(
        "INSERT IGNORE INTO v VALUES ('3.7abc'),('2.4'),('-2.5'),('2.5'),('1e3'),('1e3x'),\
         ('abc'),('-'),('0x10'),('  9  '),('  -42xyz'),('+7a'),('.5'),('')",
    )
    .unwrap();
    assert_eq!(
        cells(&mut e, "SELECT i FROM v"),
        "4,2,-3,3,1000,1000,0,0,0,9,-42,7,1,0"
    );
}

#[test]
fn round434_over_long_string_truncates() {
    let mut e = mysql();
    e.execute("CREATE TABLE t(s VARCHAR(3))").unwrap();
    e.execute("INSERT IGNORE INTO t VALUES ('toolong')")
        .unwrap();
    assert_eq!(cells(&mut e, "SELECT s FROM t"), "too");
}

#[test]
fn round434_null_into_not_null_becomes_the_type_default() {
    let mut e = mysql();
    e.execute("CREATE TABLE t(n INT NOT NULL, s VARCHAR(9) NOT NULL)")
        .unwrap();
    e.execute("INSERT IGNORE INTO t(n,s) VALUES (NULL,NULL)")
        .unwrap();
    assert_eq!(cells(&mut e, "SELECT n,s FROM t"), "0|");
}

#[test]
fn round434_out_of_range_integer_clamps() {
    let mut e = mysql();
    e.execute("CREATE TABLE t(i INT)").unwrap();
    e.execute("INSERT IGNORE INTO t VALUES (99999999999999)")
        .unwrap();
    assert_eq!(cells(&mut e, "SELECT i FROM t"), "2147483647");
}

#[test]
fn round434_clamp_respects_the_declared_mysql_width() {
    let mut e = mysql();
    e.execute("CREATE TABLE w(a TINYINT, b TINYINT UNSIGNED)")
        .unwrap();
    e.execute("INSERT IGNORE INTO w VALUES (999,-5)").unwrap();
    assert_eq!(cells(&mut e, "SELECT a,b FROM w"), "127|0");
}

#[test]
fn round434_values_that_already_fit_are_untouched() {
    let mut e = mysql();
    e.execute("CREATE TABLE t(i INT, s VARCHAR(3))").unwrap();
    e.execute("INSERT IGNORE INTO t VALUES (5,'ab')").unwrap();
    assert_eq!(cells(&mut e, "SELECT i,s FROM t"), "5|ab");
}

#[test]
fn round434_without_ignore_the_same_values_still_raise() {
    let mut e = mysql();
    e.execute("CREATE TABLE t(i INT)").unwrap();
    e.execute("INSERT INTO t VALUES ('12abc')")
        .expect_err("a plain INSERT must still reject the bad value");
    assert_eq!(cells(&mut e, "SELECT COUNT(*) FROM t"), "0");
}

#[test]
fn round434_shapes_spg_cannot_represent_stay_loud() {
    // MariaDB stores '0000-00-00' / the ENUM error-member here. SPG has
    // neither, so it must keep raising rather than store something else.
    let mut e = mysql();
    e.execute("CREATE TABLE t(d DATE, en ENUM('a','b'))")
        .unwrap();
    e.execute("INSERT IGNORE INTO t(d) VALUES ('2020-02-30')")
        .expect_err("no zero-date representation");
    e.execute("INSERT IGNORE INTO t(en) VALUES ('zzz')")
        .expect_err("no ENUM error-member representation");
}

#[test]
fn round434_pg_dialect_is_untouched() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p(i INT, n INT NOT NULL)").unwrap();
    e.execute("INSERT INTO p(i,n) VALUES ('12abc',1)")
        .expect_err("PG must still reject a non-numeric string for an integer");
}
