//! v7.38 (read01 P3.19) — SET LOCAL is transaction-scoped: it reverts at
//! COMMIT / ROLLBACK and unwinds with ROLLBACK TO SAVEPOINT, matching PG.
//! Verified against live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn show(e: &mut Engine, param: &str) -> String {
    match e.execute(&format!("SHOW {param}")).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => panic!("expected text, got {v:?}"),
        },
        o => panic!("expected rows, got {o:?}"),
    }
}

fn run(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn set_local_reverts_at_commit_and_rollback() {
    let mut e = Engine::new();
    run(&mut e, "SET work_mem = '8MB'");

    run(&mut e, "BEGIN");
    run(&mut e, "SET LOCAL work_mem = '64MB'");
    assert_eq!(show(&mut e, "work_mem"), "64MB"); // visible inside the txn
    run(&mut e, "COMMIT");
    assert_eq!(show(&mut e, "work_mem"), "8MB"); // reverts at COMMIT

    run(&mut e, "BEGIN");
    run(&mut e, "SET LOCAL work_mem = '128MB'");
    run(&mut e, "ROLLBACK");
    assert_eq!(show(&mut e, "work_mem"), "8MB"); // reverts at ROLLBACK

    // Outside a transaction block SET LOCAL has no lasting effect (PG).
    run(&mut e, "SET LOCAL work_mem = '256MB'");
    assert_eq!(show(&mut e, "work_mem"), "8MB");

    // Plain SET still persists for the session.
    run(&mut e, "SET work_mem = '16MB'");
    assert_eq!(show(&mut e, "work_mem"), "16MB");
}

#[test]
fn set_local_unwinds_with_rollback_to_savepoint() {
    // Mirrors a live PG 18.4 session exactly.
    let mut e = Engine::new();
    run(&mut e, "SET work_mem = '16MB'");
    run(&mut e, "BEGIN");
    run(&mut e, "SET LOCAL work_mem = '32MB'");
    run(&mut e, "SAVEPOINT sp");
    run(&mut e, "SET LOCAL work_mem = '512MB'");
    assert_eq!(show(&mut e, "work_mem"), "512MB");
    run(&mut e, "ROLLBACK TO sp");
    assert_eq!(show(&mut e, "work_mem"), "32MB"); // the post-sp SET LOCAL is undone
    run(&mut e, "COMMIT");
    assert_eq!(show(&mut e, "work_mem"), "16MB"); // the remaining SET LOCAL expires
}
