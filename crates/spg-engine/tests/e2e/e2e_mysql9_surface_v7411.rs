//! v7.40.11 — the MySQL surface, re-measured against the release SPG
//! says it is.
//!
//! SPG advertises `9.7.2-spg`; the oracle is pinned to `mysql:9.7.2`,
//! the newest of the 9.x community line (the 26.x year-line is Oracle's
//! `innovation` tag and this project does not ride it). Several
//! comments in the tree still described the `8.0.0-spg-v…` string SPG
//! dropped in v7.39, so this round re-measured what the 9.x surface
//! actually answers.
//!
//! Two findings, both of the same shape this file's neighbours keep
//! recording — a variable that moved between releases:
//!
//! ```text
//!                                     MySQL 9.7.2        SPG 7.40.10
//!   @@default_authentication_plugin   Unknown variable   Unknown variable  ok
//!   @@authentication_policy           *,,                Unknown variable  MISSING
//!   SHOW VARIABLES LIKE 'authentic…'  one row            no rows           MISSING
//! ```
//!
//! `default_authentication_plugin` was REMOVED and
//! `authentication_policy` replaced it, so asking 9.7.2 which plugin a
//! new account gets means reading the new name. SPG already declined
//! the removed one — the `tx_isolation` (8.0.3) and `have_ssl` (8.0.26)
//! work in `e2e_mysql_identity_v739` is the same class — and had never
//! gained the replacement.
//!
//! Everything else measured on stock `mysql:9.7.2` was unchanged from
//! the 8.0 line, including the default collation
//! `utf8mb4_0900_ai_ci` — which matters, because the NO PAD rule this
//! repository calibrated against it is derived from that default.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql_engine() -> Engine {
    let mut e = Engine::new();
    e.set_mysql_dialect(true);
    e
}

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match rows.first().map(|r| r.values[0].clone()) {
            Some(Value::Text(t)) => t.to_string(),
            other => panic!("{sql}: {other:?}"),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

/// The variable that replaced `default_authentication_plugin`. Both
/// surfaces, because one question with two answers is what this file's
/// neighbours were written for.
#[test]
fn authentication_policy_answers_on_both_surfaces() {
    let mut e = mysql_engine();
    assert_eq!(
        one(&mut e, "SELECT @@authentication_policy"),
        "*,,",
        "MySQL 9.7.2 answers `*,,`"
    );
    let listed = match e
        .execute("SHOW VARIABLES LIKE 'authentication_policy'")
        .expect("show")
    {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| (format!("{:?}", r.values[0]), format!("{:?}", r.values[1])))
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        listed.len(),
        1,
        "the inventory must list it too: {listed:?}"
    );
    assert!(listed[0].1.contains("*,,"), "{listed:?}");
}

/// And the one it replaced stays gone. A server that answers a variable
/// its own version removed is telling a client it is an older release.
#[test]
fn the_variable_it_replaced_stays_removed() {
    let mut e = mysql_engine();
    let err = e
        .execute("SELECT @@default_authentication_plugin")
        .expect_err("removed in 8.0.27, absent from 9.7.2");
    assert!(
        format!("{err}").contains("default_authentication_plugin"),
        "{err}"
    );
    match e
        .execute("SHOW VARIABLES LIKE 'default_authentication_plugin'")
        .expect("show")
    {
        QueryResult::Rows { rows, .. } => assert!(rows.is_empty(), "{rows:?}"),
        other => panic!("{other:?}"),
    }
}

/// The version SPG reports is the one the oracle is pinned to, on every
/// surface that answers it. Three answers to one question is what
/// v7.39 fixed here; this keeps them equal as the pin moves.
#[test]
fn every_surface_reports_the_same_version() {
    let mut e = mysql_engine();
    assert_eq!(one(&mut e, "SELECT VERSION()"), "9.7.2-spg");
    assert_eq!(one(&mut e, "SELECT @@version"), "9.7.2-spg");
    // The inventory surface too — `SHOW VARIABLES` answers
    // (Variable_name, Value), so the version is the SECOND column.
    match e
        .execute("SHOW VARIABLES LIKE 'version'")
        .expect("show variables")
    {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert_eq!(rows[0].values[1], Value::text("9.7.2-spg".to_string()));
        }
        other => panic!("{other:?}"),
    }
    // And the constant itself names the 9.x line, so a future bump that
    // forgets one of the surfaces fails here rather than on a customer's
    // driver.
    assert!(
        spg_engine::MYSQL_SERVER_VERSION.starts_with("9."),
        "SPG rides the 9.x community line: {}",
        spg_engine::MYSQL_SERVER_VERSION
    );
}

/// The default collation, which the NO PAD rule is derived from.
/// Measured on stock `mysql:9.7.2`: `utf8mb4_0900_ai_ci`, unchanged
/// from 8.0.
#[test]
fn the_default_collation_is_still_the_one_the_pad_rule_assumes() {
    let mut e = mysql_engine();
    assert_eq!(
        one(&mut e, "SELECT @@collation_server"),
        "utf8mb4_0900_ai_ci"
    );
    assert_eq!(one(&mut e, "SELECT @@character_set_server"), "utf8mb4");
}
