//! v7.39 (round 293, E3 Phase 1a) — the row-locking clause reaches the AST.
//!
//! `FOR UPDATE` and friends have parsed since v7.17 and been DISCARDED,
//! so SPG accepted the entire syntax and locked nothing. Measured
//! against live PG 18.4 with two sessions: while A holds
//! `SELECT … WHERE id=1 FOR UPDATE`, PG answers B's
//! `… LIMIT 1 FOR UPDATE SKIP LOCKED` with row 2 and SPG answers with
//! row 1 — two workers on the classic queue take both take the same row.
//!
//! The lock manager itself already exists and is complete
//! (`spg-engine/src/locks.rs`: PG's 4x4 conflict matrix, all three wait
//! policies, deadlock victim selection, release wired into COMMIT and
//! ROLLBACK). Nothing ever called it, because the clause never survived
//! the parse. This slice makes it survive; the engine wiring is Phase 1b.

use spg_sql::ast::{LockStrength as LS, LockWait as LW, Statement};

fn locking(sql: &str) -> Option<(LS, LW, Vec<String>)> {
    let stmt = spg_sql::parser::parse_statement(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    let Statement::Select(s) = stmt else {
        panic!("expected a SELECT from {sql}");
    };
    s.locking.map(|c| (c.strength, c.policy, c.of_tables))
}

#[test]
fn all_four_strengths_are_distinguished() {
    assert_eq!(locking("SELECT 1 FOR UPDATE").unwrap().0, LS::Update);
    assert_eq!(
        locking("SELECT 1 FOR NO KEY UPDATE").unwrap().0,
        LS::NoKeyUpdate,
    );
    assert_eq!(locking("SELECT 1 FOR SHARE").unwrap().0, LS::Share);
    assert_eq!(locking("SELECT 1 FOR KEY SHARE").unwrap().0, LS::KeyShare);
}

#[test]
fn all_three_wait_policies_are_distinguished() {
    assert_eq!(locking("SELECT 1 FOR UPDATE").unwrap().1, LW::Wait);
    assert_eq!(locking("SELECT 1 FOR UPDATE NOWAIT").unwrap().1, LW::NoWait);
    assert_eq!(
        locking("SELECT 1 FOR UPDATE SKIP LOCKED").unwrap().1,
        LW::SkipLocked,
    );
}

#[test]
fn a_select_without_the_clause_carries_nothing() {
    assert!(locking("SELECT 1").is_none());
    assert!(locking("SELECT * FROM t WHERE a = 1 ORDER BY a LIMIT 5").is_none());
}

#[test]
fn the_clause_survives_the_trailers_around_it() {
    // The clause sits after ORDER BY / LIMIT, which is where a naive
    // parser loses it.
    let (s, p, _) = locking("SELECT * FROM t ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED").unwrap();
    assert_eq!(s, LS::Update);
    assert_eq!(p, LW::SkipLocked);
}

#[test]
fn the_of_list_is_accepted() {
    // `FOR UPDATE OF t` locks only t's rows. Phase 1a keeps the slot;
    // honouring it is Phase 1b's job, and an unhonoured list must not
    // silently look like "lock everything".
    assert!(locking("SELECT * FROM t FOR UPDATE OF t").is_some());
}
