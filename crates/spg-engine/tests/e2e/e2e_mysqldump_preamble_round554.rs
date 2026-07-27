//! v7.39 (round 554) — no mysqldump could be restored past its fifth line.
//!
//! The audit's phase 6 carries V-3: the biz gates (dump_compat /
//! data_compat) have been environment-blocked for rounds, leaving the
//! project's own six-gate release discipline running on five. The
//! blocker was a local Gatekeeper hang; run on the testbed the gate
//! went straight through — and reported 14 failing fixtures with ONE
//! cause.
//!
//! Every MySQL and MariaDB dump begins:
//!
//!     /*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */;
//!     /*!40101 SET @OLD_CHARACTER_SET_RESULTS=@@CHARACTER_SET_RESULTS */;
//!     /*!40101 SET @OLD_COLLATION_CONNECTION=@@COLLATION_CONNECTION */;
//!     /*!40101 SET NAMES utf8mb4 */;
//!     /*!40103 SET @OLD_TIME_ZONE=@@TIME_ZONE */;      <- ERROR here
//!
//! `Unknown system variable 'time_zone'`. So SPG — a MySQL drop-in —
//! could not restore any mysqldump at all, and the gate that would have
//! said so had not run in rounds.
//!
//! Two things were missing. The variables the preamble reads back
//! (MariaDB 11 readings: time_zone SYSTEM, system_time_zone UTC,
//! unique_checks / foreign_key_checks / sql_notes /
//! sql_quote_show_create 1, note_verbosity `basic,explain`,
//! innodb_stats_on_metadata 0), and the SHAPE the preamble writes:
//!
//!     SET @OLD_SQL_MODE=@@SQL_MODE, SQL_MODE='NO_AUTO_VALUE_ON_ZERO'
//!
//! saves a value and changes a setting in ONE statement, and the parser
//! refused the mixture outright — "cannot mix `@@` settings with `@`
//! user variables in one SET". The trailing half is applied for real,
//! not swallowed: SQL_MODE without a STRICT_ flag turns strictness off,
//! which is what the rest of the restore then depends on.
//!
//! dump-compat went from 14 failing fixtures to none.
//!
//! Every expectation below is a MariaDB 11 reading.

use spg_engine::{Engine, QueryResult};

fn mysql_session() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .unwrap_or_default(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// The variables a dump preamble reads back.
#[test]
fn round554_preamble_variables_answer() {
    let mut e = mysql_session();
    for (expr, want) in [
        ("@@time_zone", "SYSTEM"),
        ("@@system_time_zone", "UTC"),
        ("@@unique_checks", "1"),
        ("@@foreign_key_checks", "1"),
        ("@@sql_notes", "1"),
        ("@@sql_quote_show_create", "1"),
        ("@@note_verbosity", "basic,explain"),
        ("@@innodb_stats_on_metadata", "0"),
    ] {
        assert_eq!(one(&mut e, &format!("SELECT {expr}")), want, "{expr}");
    }
    // An unknown one still raises, as MariaDB's does.
    assert!(e.execute("SELECT @@no_such_variable_at_all").is_err());
}

/// The whole preamble, as mariadb-dump 11 emits it.
#[test]
fn round554_the_whole_preamble_runs() {
    let mut e = mysql_session();
    for sql in [
        "SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT",
        "SET @OLD_CHARACTER_SET_RESULTS=@@CHARACTER_SET_RESULTS",
        "SET @OLD_COLLATION_CONNECTION=@@COLLATION_CONNECTION",
        "SET NAMES utf8mb4",
        "SET @OLD_TIME_ZONE=@@TIME_ZONE",
        "SET TIME_ZONE='+00:00'",
        "SET @OLD_UNIQUE_CHECKS=@@UNIQUE_CHECKS, UNIQUE_CHECKS=0",
        "SET @OLD_FOREIGN_KEY_CHECKS=@@FOREIGN_KEY_CHECKS, FOREIGN_KEY_CHECKS=0",
        "SET @OLD_SQL_MODE=@@SQL_MODE, SQL_MODE='NO_AUTO_VALUE_ON_ZERO'",
        "SET @OLD_NOTE_VERBOSITY=@@NOTE_VERBOSITY, NOTE_VERBOSITY=0",
    ] {
        e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
    }
    // The saved values are in the user-variable namespace, ready for the
    // restore tail that puts them back.
    assert_eq!(one(&mut e, "SELECT @OLD_TIME_ZONE"), "SYSTEM");
    assert_eq!(one(&mut e, "SELECT @OLD_UNIQUE_CHECKS"), "1");
}

/// The trailing half is APPLIED, not swallowed — SQL_MODE without a
/// STRICT_ flag turns strictness off, and the rest of a restore depends
/// on that.
#[test]
fn round554_the_setting_half_takes_effect() {
    let mut e = mysql_session();
    e.execute("CREATE TABLE t (v VARCHAR(3))").unwrap();
    // Strict: an over-long value is refused.
    assert!(e.execute("INSERT INTO t VALUES ('abcdef')").is_err());
    e.execute("SET @OLD_SQL_MODE=@@SQL_MODE, SQL_MODE='NO_AUTO_VALUE_ON_ZERO'")
        .unwrap();
    // Not strict: MariaDB truncates instead.
    e.execute("INSERT INTO t VALUES ('abcdef')").unwrap();
    assert_eq!(one(&mut e, "SELECT v FROM t"), "abc");
    // And the saved value is what the tail restores.
    assert!(one(&mut e, "SELECT @OLD_SQL_MODE").contains("STRICT_TRANS_TABLES"));
}

/// A plain multi-assignment SET still works both ways round.
#[test]
fn round554_plain_forms_unchanged() {
    let mut e = mysql_session();
    e.execute("SET @a = 1, @b = 2").unwrap();
    assert_eq!(one(&mut e, "SELECT @a, @b"), "1|2");
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    assert!(one(&mut e, "SELECT @@sql_mode").contains("STRICT"));
}
