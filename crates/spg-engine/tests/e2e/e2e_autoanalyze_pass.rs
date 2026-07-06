//! v7.37.22 (22.3) — autoanalyze_pass walks
//! tables_needing_analyze() and runs ANALYZE on each candidate.
//! The host (spg-embedded / spg-server) calls this on a timer
//! (default 60s, matching PG's autovacuum_naptime).

use spg_engine::Engine;

#[test]
fn autoanalyze_pass_returns_empty_when_no_modifications() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    // Empty table — modified count 0 — autoanalyze finds nothing.
    let analyzed = e.autoanalyze_pass().unwrap();
    assert!(analyzed.is_empty(), "got {analyzed:?}");
}

#[test]
fn autoanalyze_pass_picks_up_tables_with_pending_modifications() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    // Insert enough rows to cross PG's threshold 50 + ceil(0.1 × rows):
    // at 60 inserts modified = row_count = 60 > 50 + 6 = 56.
    for i in 0..60 {
        e.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
    }
    let analyzed = e.autoanalyze_pass().unwrap();
    assert_eq!(analyzed.len(), 1, "expected one analyzed table, got {analyzed:?}");
    assert_eq!(analyzed[0], "t");
    // After autoanalyze, the candidate set is empty again.
    let second = e.autoanalyze_pass().unwrap();
    assert!(
        second.is_empty(),
        "back-to-back autoanalyze should reset the candidate set, got {second:?}"
    );
}

#[test]
fn autoanalyze_pass_skips_internal_tables() {
    // The internal-name filter is wired through
    // tables_needing_analyze; autoanalyze_pass inherits it.
    // No real internal table here — we just confirm the
    // function doesn't blow up on an engine with only user
    // tables.
    let mut e = Engine::new();
    e.execute("CREATE TABLE users (id INT)").unwrap();
    // 60 > PG threshold 50 + ceil(60/10) = 56 → the user table analyzes.
    for i in 0..60 {
        e.execute(&format!("INSERT INTO users VALUES ({i})")).unwrap();
    }
    let analyzed = e.autoanalyze_pass().unwrap();
    assert_eq!(analyzed, vec!["users".to_string()]);
}
