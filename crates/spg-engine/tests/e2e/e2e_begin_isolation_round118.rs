//! v7.39 (read01 round 118, B3) — `BEGIN ISOLATION LEVEL …` applies the level
//! for the transaction, and it reverts to the default at COMMIT / ROLLBACK.
//!
//! The parser used to discard the BEGIN isolation clause (`let _ = …`), so the
//! level never reached the engine and `SHOW transaction_isolation` was stuck at
//! the engine-global value. `Statement::Begin` now carries
//! `Option<IsolationLevel>`, `exec_begin` applies it before caching the RR/SER
//! snapshot, and both COMMIT and ROLLBACK reset the level. Locked byte-identical
//! against PG 18.4 (single session).

use spg_engine::{Engine, QueryResult};

fn show_iso(e: &mut Engine) -> String {
    match e
        .execute("SHOW transaction_isolation")
        .expect("SHOW transaction_isolation")
    {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    }
}

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"));
}

#[test]
fn begin_isolation_level_applies_and_reverts() {
    let mut e = Engine::new();
    assert_eq!(show_iso(&mut e), "read committed");

    ok(&mut e, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(show_iso(&mut e), "repeatable read");
    ok(&mut e, "COMMIT");
    assert_eq!(show_iso(&mut e), "read committed");

    // START TRANSACTION synonym + ROLLBACK reset.
    ok(&mut e, "START TRANSACTION ISOLATION LEVEL SERIALIZABLE");
    assert_eq!(show_iso(&mut e), "serializable");
    ok(&mut e, "ROLLBACK");
    assert_eq!(show_iso(&mut e), "read committed");

    // A bare BEGIN keeps the default; a non-isolation mode does too.
    ok(&mut e, "BEGIN READ ONLY");
    assert_eq!(show_iso(&mut e), "read committed");
    ok(&mut e, "COMMIT");
}
