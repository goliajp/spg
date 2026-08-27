//! v7.39.2 — one relation, whichever way its name is spelled.
//!
//! The lexer folds an unquoted identifier and leaves a backticked one
//! alone, so in a MySQL session `CREATE TABLE MyTable` stored `mytable`
//! while ``SELECT 1 FROM `MyTable` `` looked for `MyTable` and found
//! nothing. Two spellings of one name were two tables. `mysqldump`
//! backticks every identifier, so a dump restored here and an
//! application that writes the name unquoted were looking at different
//! relations.
//!
//! MySQL calls this `lower_case_table_names = 1` — compare without
//! case — and SPG reports `1` now rather than the `0` it used to claim,
//! which asserted a case-sensitivity it never performed.
//!
//! PostgreSQL is untouched: `"MyTbl"` and `mytbl` are two relations
//! there, measured on 18.6, and both can exist at once.
//!
//! The comparison is per SESSION while the catalog is shared by all of
//! them, which is the shape that has gone wrong repeatedly in this
//! codebase — so two of these run two sessions, and one runs a
//! transaction, because a shadow catalog is a second copy of the flag.

use spg_engine::{Engine, IMPLICIT_TX, QueryResult, TxId};

fn ask(e: &mut Engine, session: u32, tx: TxId, sql: &str) -> Result<String, String> {
    e.set_current_session(session);
    match e.execute_in(sql, tx) {
        Err(err) => Err(format!("{err}")),
        Ok(QueryResult::Rows { rows, .. }) if !rows.is_empty() => {
            Ok(spg_engine::eval::value_to_text(&rows[0].values[0]))
        }
        Ok(_) => Ok("<ok>".to_string()),
    }
}

fn mysql_session(e: &mut Engine, session: u32, tx: TxId) {
    ask(e, session, tx, "SET sql_mode=''").expect("entering the MySQL dialect");
}

#[test]
fn a_mysql_session_finds_the_table_under_either_spelling() {
    let mut e = Engine::new();
    mysql_session(&mut e, 1, IMPLICIT_TX);
    ask(&mut e, 1, IMPLICIT_TX, "CREATE TABLE MyTable (MyCol int)").unwrap();
    ask(&mut e, 1, IMPLICIT_TX, "INSERT INTO MyTable VALUES (7)").unwrap();
    for spelling in ["MyTable", "`MyTable`", "mytable", "MYTABLE"] {
        let sql = format!("SELECT MyCol FROM {spelling}");
        assert_eq!(
            ask(&mut e, 1, IMPLICIT_TX, &sql).unwrap(),
            "7",
            "{sql} must reach the one table"
        );
    }
}

#[test]
fn a_table_created_with_its_case_kept_is_still_reachable() {
    // A backticked CREATE stores the name as written. Nothing about
    // that changes here — what changes is that the lower-case spelling
    // now finds it, so an existing table does not become unreachable.
    let mut e = Engine::new();
    mysql_session(&mut e, 1, IMPLICIT_TX);
    ask(&mut e, 1, IMPLICIT_TX, "CREATE TABLE `KeepCase` (a int)").unwrap();
    ask(&mut e, 1, IMPLICIT_TX, "INSERT INTO `KeepCase` VALUES (3)").unwrap();
    assert_eq!(
        ask(&mut e, 1, IMPLICIT_TX, "SELECT a FROM keepcase").unwrap(),
        "3"
    );
    assert_eq!(
        ask(&mut e, 1, IMPLICIT_TX, "SELECT a FROM KEEPCASE").unwrap(),
        "3"
    );
}

#[test]
fn a_postgresql_session_keeps_two_relations_that_differ_only_in_case() {
    // Measured on PG 18.6: both are created, each found only by its own
    // spelling. A fold applied here would merge them.
    let mut e = Engine::new();
    ask(&mut e, 1, IMPLICIT_TX, r#"CREATE TABLE "MyTbl" (a int)"#).unwrap();
    ask(&mut e, 1, IMPLICIT_TX, "CREATE TABLE mytbl (a int)").unwrap();
    ask(&mut e, 1, IMPLICIT_TX, r#"INSERT INTO "MyTbl" VALUES (1)"#).unwrap();
    assert_eq!(
        ask(&mut e, 1, IMPLICIT_TX, r#"SELECT count(*) FROM "MyTbl""#).unwrap(),
        "1"
    );
    assert_eq!(
        ask(&mut e, 1, IMPLICIT_TX, "SELECT count(*) FROM mytbl").unwrap(),
        "0",
        "the unquoted name is the OTHER table"
    );
    assert!(
        ask(&mut e, 1, IMPLICIT_TX, r#"SELECT 1 FROM "NoSuchTbl""#)
            .unwrap_err()
            .contains("NoSuchTbl"),
        "and a name that exists in no case at all still errors"
    );
}

#[test]
fn one_sessions_dialect_does_not_reach_another() {
    // The catalog is shared and the comparison is not. A MySQL session
    // being open must not make a PostgreSQL session fold — that is the
    // shape of defect this release has already found twice elsewhere.
    let mut e = Engine::new();
    let a = e.alloc_tx_id();
    let b = e.alloc_tx_id();

    // B speaks MySQL and makes a table.
    mysql_session(&mut e, 2, b);
    ask(&mut e, 2, b, "CREATE TABLE Shared (a int)").unwrap();

    // A is a PostgreSQL session: `Shared` folds to `shared`, which the
    // catalog holds, so A finds it — that much is PG's own rule.
    assert!(ask(&mut e, 1, a, "SELECT count(*) FROM shared").is_ok());
    // But a quoted mixed-case name must NOT resolve for A.
    assert!(
        ask(&mut e, 1, a, r#"SELECT count(*) FROM "SHARED""#)
            .unwrap_err()
            .contains("SHARED"),
        "a PG session must not fold just because a MySQL session is open"
    );
    // And B still does fold.
    assert!(ask(&mut e, 2, b, "SELECT count(*) FROM `SHARED`").is_ok());
}

#[test]
fn the_comparison_reaches_inside_a_transaction() {
    // A transaction runs against a SHADOW copy of the catalog, which is
    // a second place the per-session flag has to be installed. Without
    // it, the same query answers differently inside BEGIN.
    let mut e = Engine::new();
    let tx = e.alloc_tx_id();
    mysql_session(&mut e, 1, tx);
    ask(&mut e, 1, tx, "CREATE TABLE TxCase (a int)").unwrap();
    ask(&mut e, 1, tx, "INSERT INTO TxCase VALUES (5)").unwrap();
    ask(&mut e, 1, tx, "BEGIN").unwrap();
    assert_eq!(
        ask(&mut e, 1, tx, "SELECT a FROM `TxCase`").unwrap(),
        "5",
        "inside a transaction too"
    );
    ask(&mut e, 1, tx, "COMMIT").unwrap();
    assert_eq!(ask(&mut e, 1, tx, "SELECT a FROM `TxCase`").unwrap(), "5");
}

#[test]
fn another_session_connecting_does_not_change_an_open_transaction() {
    // A shadow catalog is a SECOND copy of the per-session comparison,
    // and the first version of this installed the current session's
    // answer into every open transaction's shadow — so a PostgreSQL
    // session sitting inside BEGIN began folding the moment a MySQL
    // client connected.
    //
    // Found by an ablation that did not bite: removing the shadow
    // installation left every pin green, which said no pin reached it.
    //
    // The probe is a SUCCESSFUL read, not a failing one: a statement
    // that errors inside a transaction aborts the block, and every
    // later statement then answers about the abort rather than about
    // the name. The first draft of this test asked twice and read the
    // second answer as the defect it was looking for.
    let mut e = Engine::new();
    let a = e.alloc_tx_id();
    let b = e.alloc_tx_id();

    // A is PostgreSQL and holds two relations differing only in case,
    // so folding would change which one a read reaches — visibly, and
    // without erroring.
    ask(&mut e, 1, a, r#"CREATE TABLE "CaseA" (a int)"#).unwrap();
    ask(&mut e, 1, a, "CREATE TABLE casea (a int)").unwrap();
    ask(&mut e, 1, a, r#"INSERT INTO "CaseA" VALUES (1), (2)"#).unwrap();
    ask(&mut e, 1, a, "BEGIN").unwrap();
    assert_eq!(
        ask(&mut e, 1, a, "SELECT count(*) FROM casea").unwrap(),
        "0",
        "the unquoted name is the empty table"
    );

    // B connects and speaks MySQL.
    mysql_session(&mut e, 2, b);
    ask(&mut e, 2, b, "SELECT 1").unwrap();

    // A, still inside its transaction, must still reach the same one.
    assert_eq!(
        ask(&mut e, 1, a, "SELECT count(*) FROM casea").unwrap(),
        "0",
        "a MySQL connection must not reach into another session's transaction"
    );
    ask(&mut e, 1, a, "COMMIT").unwrap();
}

#[test]
fn entering_the_mysql_dialect_inside_a_transaction_takes_effect_there() {
    // The shadow catalog is cloned at BEGIN, so it carries whatever the
    // session's comparison was THEN. A session that enters the MySQL
    // dialect after opening its transaction has to have the new answer
    // installed into that shadow — which is the only thing the shadow
    // write in `refresh_name_folding` is for.
    //
    // The first draft of this file had no test that reached it: the
    // ablation removing the shadow write left every pin green.
    let mut e = Engine::new();
    let tx = e.alloc_tx_id();
    ask(&mut e, 1, tx, "CREATE TABLE MidTx (a int)").unwrap();
    ask(&mut e, 1, tx, "INSERT INTO MidTx VALUES (9)").unwrap();
    ask(&mut e, 1, tx, "BEGIN").unwrap();
    // Still PostgreSQL here, so the stored name is `midtx` and a
    // quoted mixed-case spelling does not reach it.
    assert!(
        ask(&mut e, 1, tx, r#"SELECT a FROM "MidTx""#)
            .unwrap_err()
            .contains("MidTx")
    );
    ask(&mut e, 1, tx, "ROLLBACK").unwrap();

    ask(&mut e, 1, tx, "BEGIN").unwrap();
    ask(&mut e, 1, tx, "SET sql_mode=''").unwrap();
    assert_eq!(
        ask(&mut e, 1, tx, "SELECT a FROM `MidTx`").unwrap(),
        "9",
        "the dialect entered inside the transaction governs inside it"
    );
    ask(&mut e, 1, tx, "COMMIT").unwrap();
}

#[test]
fn lower_case_table_names_reports_what_the_session_actually_does() {
    let mut e = Engine::new();
    // A PostgreSQL session does not fold.
    assert_eq!(
        ask(&mut e, 1, IMPLICIT_TX, "SELECT @@lower_case_table_names").unwrap(),
        "0"
    );
    mysql_session(&mut e, 1, IMPLICIT_TX);
    assert_eq!(
        ask(&mut e, 1, IMPLICIT_TX, "SELECT @@lower_case_table_names").unwrap(),
        "1",
        "MySQL's own name for comparing without case"
    );
}
