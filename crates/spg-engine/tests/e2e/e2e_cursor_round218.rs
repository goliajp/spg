//! v7.39 (round 218) — server-side cursors (DECLARE / FETCH / MOVE /
//! CLOSE), the canonical driver path for streaming large result sets
//! (psycopg2 named cursors, JDBC setFetchSize). Live-PG18.4 differential
//! (2026-07-18) pinned every behaviour asserted here:
//!   - DECLARE outside a tx block errors (25P01 wording)
//!   - FETCH NEXT / n / ALL walk forward; past-the-end fetch = 0 rows
//!   - a DEFAULT cursor allows backward fetch (PG only rejects it for
//!     explicit NO SCROLL: "cursor can only scan forward" + HINT)
//!   - SCROLL: PRIOR / FIRST / LAST / ABSOLUTE k / RELATIVE k / BACKWARD
//!   - WITH HOLD survives COMMIT; ROLLBACK closes unheld cursors (34000
//!     on subsequent FETCH); unknown cursor name = 34000
//!   - MOVE = FETCH without rows (count in `affected`)

use spg_engine::{Engine, QueryResult};

fn rows_of(r: QueryResult) -> Vec<Vec<String>> {
    match r {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        spg_storage::Value::Null => "NULL".to_string(),
                        spg_storage::Value::Int(n) => n.to_string(),
                        other => format!("{other:?}"),
                    })
                    .collect()
            })
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE c (id int)").unwrap();
    e.execute("INSERT INTO c VALUES (1),(2),(3),(4),(5)").unwrap();
    e
}

#[test]
fn declare_requires_transaction_block() {
    let mut e = seeded();
    let err = e
        .execute("DECLARE nope CURSOR FOR SELECT id FROM c")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("DECLARE CURSOR can only be used in transaction blocks"),
        "{err}"
    );
}

#[test]
fn forward_walk_matches_pg() {
    let mut e = seeded();
    e.execute("BEGIN").unwrap();
    e.execute("DECLARE cur CURSOR FOR SELECT id FROM c ORDER BY id")
        .unwrap();
    assert_eq!(
        rows_of(e.execute("FETCH NEXT FROM cur").unwrap()),
        vec![vec!["1".to_string()]]
    );
    assert_eq!(
        rows_of(e.execute("FETCH 2 FROM cur").unwrap()),
        vec![vec!["2".to_string()], vec!["3".to_string()]]
    );
    // MOVE 1 skips row 4; count rides `affected`.
    match e.execute("MOVE 1 IN cur").unwrap() {
        QueryResult::CommandOk { affected, .. } => assert_eq!(affected, 1),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        rows_of(e.execute("FETCH NEXT FROM cur").unwrap()),
        vec![vec!["5".to_string()]]
    );
    // Exhausted: FETCH ALL returns 0 rows, no error.
    assert_eq!(
        rows_of(e.execute("FETCH ALL FROM cur").unwrap()),
        Vec::<Vec<String>>::new()
    );
    e.execute("CLOSE cur").unwrap();
    e.execute("COMMIT").unwrap();
}

#[test]
fn scroll_directions_match_pg() {
    let mut e = seeded();
    e.execute("BEGIN").unwrap();
    e.execute("DECLARE sc SCROLL CURSOR FOR SELECT id FROM c ORDER BY id")
        .unwrap();
    let one = |e: &mut Engine, sql: &str| rows_of(e.execute(sql).unwrap());
    assert_eq!(one(&mut e, "FETCH LAST FROM sc"), vec![vec!["5".to_string()]]);
    assert_eq!(one(&mut e, "FETCH PRIOR FROM sc"), vec![vec!["4".to_string()]]);
    assert_eq!(one(&mut e, "FETCH ABSOLUTE 2 FROM sc"), vec![vec!["2".to_string()]]);
    assert_eq!(one(&mut e, "FETCH BACKWARD 1 FROM sc"), vec![vec!["1".to_string()]]);
    // ABSOLUTE 0 = before first → 0 rows; RELATIVE 2 from there → row 2.
    assert_eq!(one(&mut e, "FETCH ABSOLUTE 0 FROM sc"), Vec::<Vec<String>>::new());
    assert_eq!(one(&mut e, "FETCH RELATIVE 2 FROM sc"), vec![vec!["2".to_string()]]);
    assert_eq!(one(&mut e, "FETCH FIRST FROM sc"), vec![vec!["1".to_string()]]);
    e.execute("COMMIT").unwrap();
}

#[test]
fn default_cursor_allows_backward_no_scroll_rejects() {
    let mut e = seeded();
    e.execute("BEGIN").unwrap();
    // DEFAULT (no keyword): backward works (matches PG's simple-plan rule).
    e.execute("DECLARE d CURSOR FOR SELECT id FROM c ORDER BY id")
        .unwrap();
    e.execute("FETCH ALL FROM d").unwrap();
    assert_eq!(
        rows_of(e.execute("FETCH BACKWARD 1 FROM d").unwrap()),
        vec![vec!["5".to_string()]]
    );
    // Explicit NO SCROLL: backward errors with PG's message + HINT.
    e.execute("DECLARE ns NO SCROLL CURSOR FOR SELECT id FROM c ORDER BY id")
        .unwrap();
    e.execute("FETCH NEXT FROM ns").unwrap();
    let err = e
        .execute("FETCH BACKWARD 1 FROM ns")
        .unwrap_err()
        .to_string();
    assert!(err.contains("cursor can only scan forward"), "{err}");
    assert!(
        err.contains("Declare it with SCROLL option to enable backward scan."),
        "{err}"
    );
    e.execute("COMMIT").unwrap();
}

#[test]
fn with_hold_survives_commit_rollback_closes_unheld() {
    let mut e = seeded();
    // WITH HOLD: keeps position across COMMIT.
    e.execute("BEGIN").unwrap();
    e.execute("DECLARE h CURSOR WITH HOLD FOR SELECT id FROM c ORDER BY id")
        .unwrap();
    assert_eq!(
        rows_of(e.execute("FETCH NEXT FROM h").unwrap()),
        vec![vec!["1".to_string()]]
    );
    e.execute("COMMIT").unwrap();
    assert_eq!(
        rows_of(e.execute("FETCH NEXT FROM h").unwrap()),
        vec![vec!["2".to_string()]]
    );
    e.execute("CLOSE h").unwrap();
    // Non-hold cursor dies at ROLLBACK: subsequent FETCH = 34000.
    e.execute("BEGIN").unwrap();
    e.execute("DECLARE r CURSOR FOR SELECT id FROM c ORDER BY id")
        .unwrap();
    e.execute("ROLLBACK").unwrap();
    let err = e.execute("FETCH NEXT FROM r").unwrap_err().to_string();
    assert!(err.contains("cursor \"r\" does not exist"), "{err}");
    // Unknown name is the same error; CLOSE ALL succeeds when empty.
    let err = e.execute("FETCH 2 FROM nothere").unwrap_err().to_string();
    assert!(err.contains("cursor \"nothere\" does not exist"), "{err}");
    e.execute("CLOSE ALL").unwrap();
}

#[test]
fn hold_cursor_survives_later_rollback() {
    let mut e = seeded();
    e.execute("BEGIN").unwrap();
    e.execute("DECLARE h CURSOR WITH HOLD FOR SELECT id FROM c ORDER BY id")
        .unwrap();
    e.execute("COMMIT").unwrap(); // h is now held
    e.execute("BEGIN").unwrap();
    e.execute("ROLLBACK").unwrap(); // must NOT close h
    assert_eq!(
        rows_of(e.execute("FETCH NEXT FROM h").unwrap()),
        vec![vec!["1".to_string()]]
    );
    e.execute("CLOSE h").unwrap();
}

#[test]
fn duplicate_declare_rejected() {
    let mut e = seeded();
    e.execute("BEGIN").unwrap();
    e.execute("DECLARE dup CURSOR FOR SELECT id FROM c").unwrap();
    let err = e
        .execute("DECLARE dup CURSOR FOR SELECT id FROM c")
        .unwrap_err()
        .to_string();
    assert!(err.contains("cursor \"dup\" already exists"), "{err}");
    e.execute("COMMIT").unwrap();
}
