//! v7.39.2 — `USE <db>` records the name; `DATABASE()` answers it.
//!
//! `USE` was swallowed with the dump noise and parsed as `Empty`, so
//! `USE myapp; SELECT DATABASE()` answered the same constant it answered
//! before. Measured on MySQL 9.7.2, `DATABASE()` has three states and
//! SPG answered `spg` in all of them:
//!
//!   no database at handshake   NULL     was `spg`
//!   a database at handshake    its name was `spg`
//!   after `USE myapp`          `myapp`  was `spg`
//!
//! SPG serves ONE database and answers to any name — which is what
//! `CREATE DATABASE` already documents, and this does not change it.
//! What it changes is the NAME, which is the half a client can observe
//! and the half the PostgreSQL wire has tracked since v7.39.

use spg_engine::{Engine, QueryResult};

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.set_mysql_dialect(true);
    e
}

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) if !rows.is_empty() => {
            spg_engine::eval::value_to_text(&rows[0].values[0])
        }
        Ok(_) => "<none>".to_string(),
        Err(err) => format!("ERR {err}"),
    }
}

#[test]
fn use_names_the_database_that_database_answers() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT DATABASE()"), "NULL");
    e.execute("USE myapp").expect("USE");
    assert_eq!(one(&mut e, "SELECT DATABASE()"), "myapp");
    // And again, to a different name.
    e.execute("USE other").expect("USE");
    assert_eq!(one(&mut e, "SELECT DATABASE()"), "other");
    // `SCHEMA()` is MySQL's synonym for it.
    assert_eq!(one(&mut e, "SELECT SCHEMA()"), "other");
}

#[test]
fn one_database_is_still_served_under_every_name() {
    // The documented model, held in place: switching the NAME does not
    // switch catalogs, and a pin that only checked `DATABASE()` would
    // not notice if it started to.
    let mut e = mysql();
    e.execute("USE first").expect("USE");
    e.execute("CREATE TABLE t_anywhere (a INT)")
        .expect("create");
    e.execute("USE second").expect("USE");
    assert_eq!(
        one(&mut e, "SELECT COUNT(*) FROM t_anywhere"),
        "0",
        "the table is still there under the other name"
    );
}

#[test]
fn postgres_keeps_its_own_answers() {
    // The negative control on the other wire. `current_database()` is
    // PostgreSQL's and never NULL; `USE` is not PostgreSQL SQL at all and
    // stays swallowed there, because that swallow is for restores and
    // taking it away is a different question from this one.
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT current_database()"), "spg");
    assert_eq!(one(&mut e, "SELECT database()"), "spg");
    e.execute("USE myapp")
        .expect("swallowed, not a parse error");
    assert_eq!(one(&mut e, "SELECT current_database()"), "spg");
}
