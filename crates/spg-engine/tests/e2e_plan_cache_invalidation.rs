//! v6.3.1 — Plan cache invalidation on ANALYZE / DDL.
//!
//! - Successful ANALYZE bumps the engine-wide statistics version.
//! - `prepare_cached` lookups compare the cached entry's snapshot
//!   against the live version and evict on mismatch.
//! - Bare `ANALYZE` (all tables) clears the cache wholesale; named
//!   `ANALYZE t` evicts only plans referencing `t`.
//! - `CREATE INDEX` / `ALTER INDEX REBUILD` evict plans referencing
//!   the affected table.

use spg_engine::Engine;

fn setup() -> Engine {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE users (id INT, name TEXT)")
        .unwrap();
    eng.execute("CREATE TABLE orders (id INT, user_id INT)")
        .unwrap();
    eng.execute("INSERT INTO users VALUES (1, 'a'), (2, 'b'), (3, 'c')")
        .unwrap();
    eng.execute("INSERT INTO orders VALUES (10, 1), (20, 1), (30, 2)")
        .unwrap();
    eng
}

#[test]
fn analyze_evicts_plans_for_analyzed_table() {
    let mut eng = setup();
    eng.prepare_cached("SELECT * FROM users WHERE id = 1")
        .unwrap();
    eng.prepare_cached("SELECT * FROM orders WHERE id = 10")
        .unwrap();
    assert_eq!(eng.plan_cache().len(), 2);

    eng.execute("ANALYZE users").unwrap();

    // After ANALYZE on `users`, the `users` plan should have been
    // evicted. The `orders` plan should still be in the cache.
    assert_eq!(
        eng.plan_cache().len(),
        1,
        "named ANALYZE only evicts referencing plans"
    );

    // Forcing a re-prepare of the users SQL should add a new entry
    // with the bumped statistics_version.
    eng.prepare_cached("SELECT * FROM users WHERE id = 1")
        .unwrap();
    assert_eq!(eng.plan_cache().len(), 2);
}

#[test]
fn analyze_bare_clears_whole_cache() {
    let mut eng = setup();
    eng.prepare_cached("SELECT * FROM users WHERE id = 1")
        .unwrap();
    eng.prepare_cached("SELECT * FROM orders WHERE id = 10")
        .unwrap();
    assert_eq!(eng.plan_cache().len(), 2);

    eng.execute("ANALYZE").unwrap();

    assert_eq!(eng.plan_cache().len(), 0, "bare ANALYZE clears the cache");
}

#[test]
fn unrelated_analyze_does_not_evict() {
    let mut eng = setup();
    eng.prepare_cached("SELECT * FROM users WHERE id = 1")
        .unwrap();
    assert_eq!(eng.plan_cache().len(), 1);

    eng.execute("ANALYZE orders").unwrap();

    // The users plan must survive an ANALYZE that doesn't touch users.
    assert!(
        eng.plan_cache().len() >= 1,
        "users plan should still be cached"
    );
}

#[test]
fn statistics_version_snapshots_at_prepare_time() {
    let mut eng = setup();
    let v0 = eng_statistics_version(&eng);
    eng.prepare_cached("SELECT id FROM users").unwrap();
    eng.execute("ANALYZE users").unwrap();
    let v1 = eng_statistics_version(&eng);
    assert!(v1 > v0, "ANALYZE bumps statistics_version");

    // First prepare after the bump must miss (the prior entry had
    // version=v0 and got evicted) and produce a fresh entry at v1.
    eng.prepare_cached("SELECT id FROM users").unwrap();
    // Sanity: cache lookup returns a plan whose version matches v1.
    // We expose this via the read-only accessor.
    let plan = eng
        .plan_cache()
        .get_snapshot("SELECT id FROM users")
        .expect("cached after re-prepare");
    assert_eq!(plan.statistics_version, v1);
}

#[test]
fn create_index_evicts_plans_for_affected_table() {
    let mut eng = setup();
    eng.prepare_cached("SELECT * FROM users WHERE id = 1")
        .unwrap();
    eng.prepare_cached("SELECT * FROM orders WHERE id = 10")
        .unwrap();
    assert_eq!(eng.plan_cache().len(), 2);

    eng.execute("CREATE INDEX idx_users_id ON users (id)")
        .unwrap();

    // Users plan must have evicted; orders plan survives.
    assert_eq!(eng.plan_cache().len(), 1);
}

// ── helpers ───────────────────────────────────────────────────────

fn eng_statistics_version(eng: &Engine) -> u64 {
    eng.statistics().version()
}
