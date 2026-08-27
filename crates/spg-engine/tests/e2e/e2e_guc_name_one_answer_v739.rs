//! v7.39 — one question about a parameter name, one answer.
//!
//! Four surfaces answer "is this a parameter?" and they used to answer
//! it separately. Measured against PostgreSQL 18.6 on the same name:
//!
//! ```text
//!                                     PG 18.6                    SPG
//! SET nosuch_guc = 'x'                unrecognized …             same
//! SHOW nosuch_guc                     unrecognized …             SPG's own sentence
//! SELECT current_setting('nosuch_guc') unrecognized …            ''  (no error)
//! SELECT set_config('nosuch_guc','x',false) unrecognized …       'x' (no error)
//! ```
//!
//! Two of those report a parameter as read or applied when neither
//! happened, which is how a typo'd name goes unnoticed. The machinery to
//! answer correctly was already there — `pg_settings` carries the same
//! 399 names PG 18.6 does, and `SET` already consulted it — and exactly
//! one of the four surfaces used it.
//!
//! Nothing in the 6,622 tests of this harness went red when the other
//! three were changed to agree, which is why these exist.

use spg_engine::{Engine, QueryResult};

/// The answer, or the error text — whichever the surface gives.
fn ask(e: &mut Engine, sql: &str) -> Result<String, String> {
    match e.execute(sql) {
        Err(err) => Err(format!("{err}")),
        Ok(QueryResult::Rows { rows, .. }) if !rows.is_empty() => Ok(match &rows[0].values[0] {
            spg_storage::Value::Null => "<NULL>".to_string(),
            other => spg_engine::eval::value_to_text(other),
        }),
        Ok(_) => Ok("<ok>".to_string()),
    }
}

const PG_REFUSAL: &str = "unrecognized configuration parameter \"nosuch_guc\"";

fn refuses_like_pg(sql: &str) {
    let mut e = Engine::new();
    let got = ask(&mut e, sql);
    let Err(msg) = got else {
        panic!("{sql} answered {got:?} where PG 18.6 errors");
    };
    assert!(
        msg.contains(PG_REFUSAL),
        "{sql}: PG says `{PG_REFUSAL}`, this says `{msg}`"
    );
}

// One surface per test, so an ablation of one of them cannot hide
// behind the others going red at the same time.

#[test]
fn set_refuses_an_unknown_name() {
    refuses_like_pg("SET nosuch_guc = 'x'");
}

#[test]
fn show_refuses_an_unknown_name() {
    refuses_like_pg("SHOW nosuch_guc");
}

#[test]
fn current_setting_refuses_an_unknown_name() {
    refuses_like_pg("SELECT current_setting('nosuch_guc')");
}

#[test]
fn set_config_refuses_an_unknown_name() {
    refuses_like_pg("SELECT set_config('nosuch_guc', 'x', false)");
}

#[test]
fn the_names_that_must_keep_working_still_do() {
    // Refusing by name is only safe because the inventory is PG's own.
    // A parameter PG has, a custom dotted one, and the MySQL-dialect
    // names `mysqldump` preambles emit all stay accepted.
    let mut e = Engine::new();
    for sql in [
        "SET random_page_cost = 4",
        "SET work_mem = '64MB'",
        "SET app.tenant = '42'",
        "SELECT set_config('app.tenant', '42', false)",
        "SET sql_mode = 'STRICT_TRANS_TABLES'",
        "SET foreign_key_checks = 0",
    ] {
        ask(&mut e, sql).unwrap_or_else(|m| panic!("{sql} was refused: {m}"));
    }
    assert_eq!(
        ask(&mut e, "SELECT current_setting('app.tenant')").unwrap(),
        "42"
    );
    assert_eq!(
        ask(&mut e, "SELECT current_setting('random_page_cost')").unwrap(),
        "4"
    );
}

#[test]
fn missing_ok_is_still_null_not_an_error() {
    let mut e = Engine::new();
    assert_eq!(
        ask(&mut e, "SELECT current_setting('nosuch_guc', true)").unwrap(),
        "<NULL>"
    );
    assert_eq!(
        ask(&mut e, "SELECT current_setting('app.never_set', true)").unwrap(),
        "<NULL>",
        "a custom name this session never set does not exist — PG agrees"
    );
}

#[test]
fn a_custom_guc_this_session_set_once_stays_defined_and_reads_empty() {
    // PG 18.6: `SET app.z='1'; RESET app.z;` leaves app.z DEFINED with
    // an empty value — `current_setting('app.z', true)` is '', not NULL.
    // An application branching on `IS NULL` versus `= ''` branches the
    // other way if this is wrong.
    let mut e = Engine::new();
    ask(&mut e, "SET app.z = '1'").unwrap();
    ask(&mut e, "RESET app.z").unwrap();
    assert_eq!(
        ask(&mut e, "SELECT current_setting('app.z', true)").unwrap(),
        "",
        "RESET leaves the custom parameter defined and empty"
    );
    assert_eq!(
        ask(&mut e, "SHOW app.z").unwrap(),
        "",
        "and SHOW prints it rather than refusing the name"
    );
}

#[test]
fn a_local_custom_guc_reads_empty_after_its_transaction_ends() {
    // Same rule through the other door: the value is gone at COMMIT, the
    // name is not.
    let mut e = Engine::new();
    ask(&mut e, "BEGIN").unwrap();
    ask(&mut e, "SET LOCAL app.y = 'L'").unwrap();
    assert_eq!(ask(&mut e, "SELECT current_setting('app.y')").unwrap(), "L");
    ask(&mut e, "COMMIT").unwrap();
    assert_eq!(
        ask(&mut e, "SELECT current_setting('app.y', true)").unwrap(),
        "",
        "defined and empty after the transaction, as PG 18.6 answers"
    );
}
