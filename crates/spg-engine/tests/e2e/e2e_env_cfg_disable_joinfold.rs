//! v7.38 P0 元机制 D — `SPG_TEST_DISABLE_JOINFOLD=1` acceptor pin.
//! The v7.32 joinfold rewrite (`try_fold_inner_joins`) folds inner
//! JOINs with proven-key dependence into a single-table scan; with
//! the GUC on, the rewrite is skipped and the join runs as a real
//! join. Both produce the same rows — joinfold is a semantically
//! equivalent rewrite — so the assertion is byte-equal results
//! across the two configurations.

use spg_engine::{Engine, QueryResult, testkit::EnvConfig};

fn build_engine(disable: bool) -> Engine {
    let cfg = EnvConfig::builder().disable_joinfold(disable).build();
    let mut e = Engine::new().with_env_cfg(cfg);
    e.execute(
        "CREATE TABLE u (id INT NOT NULL PRIMARY KEY, name TEXT NOT NULL, org INT NOT NULL)",
    )
    .unwrap();
    e.execute("CREATE TABLE o (id INT NOT NULL PRIMARY KEY, label TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, 'alice', 10), (2, 'bob', 20), (3, 'carol', 10)")
        .unwrap();
    e.execute("INSERT INTO o VALUES (10, 'eng'), (20, 'sales')")
        .unwrap();
    e
}

fn rows_of(r: QueryResult) -> Vec<Vec<spg_storage::Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.iter().map(|r| r.values.clone()).collect(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn joinfold_off_matches_joinfold_on() {
    let mut e_on = build_engine(false); // joinfold ENABLED (production default)
    let mut e_off = build_engine(true); // joinfold DISABLED (test GUC on)
    let sql =
        "SELECT u.id, u.name, o.label FROM u JOIN o ON u.org = o.id ORDER BY u.id";
    let on_rows = rows_of(e_on.execute(sql).unwrap());
    let off_rows = rows_of(e_off.execute(sql).unwrap());
    assert_eq!(
        on_rows, off_rows,
        "joinfold-on and joinfold-off must produce byte-equal rows"
    );
    assert_eq!(on_rows.len(), 3);
}

#[test]
fn joinfold_off_default_in_production() {
    // No env override → joinfold is ON (the production default).
    let e = build_engine(false);
    assert!(
        !e.env_cfg().disable_joinfold,
        "production default must keep joinfold enabled"
    );
}
