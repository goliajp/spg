//! v7.37.16 (16.10 [PG+]) — EXPLAIN annotates partition-parent
//! TableScans with the names of children the planner would
//! actually scan after WHERE-clause pruning.
//!
//! PG only emits `Subplans Removed: N`; SPG's annotation puts the
//! kept names inline so dogfood-replay + sentori dashboards can
//! diff prune outcomes without re-running the query.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows for {sql}");
    };
    rows.into_iter().map(|r| r.values).collect()
}

fn explain_text(e: &mut Engine, sql: &str) -> String {
    let mut out = String::new();
    for row in rows(e, sql) {
        if let Value::Text(s) = &row[0] {
            out.push_str(s);
            out.push('\n');
        }
    }
    out
}

#[test]
fn explain_lists_kept_children_for_list_eq_predicate() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE cust (id BIGINT, region TEXT) PARTITION BY LIST (region)")
        .unwrap();
    e.execute("CREATE TABLE cust_apac PARTITION OF cust FOR VALUES IN ('jp', 'kr')")
        .unwrap();
    e.execute("CREATE TABLE cust_emea PARTITION OF cust FOR VALUES IN ('de', 'fr')")
        .unwrap();
    e.execute("CREATE TABLE cust_default PARTITION OF cust DEFAULT")
        .unwrap();

    let plan = explain_text(&mut e, "EXPLAIN SELECT * FROM cust WHERE region = 'jp'");
    assert!(
        plan.contains("[partition parent]"),
        "missing partition-parent marker: {plan}"
    );
    assert!(
        plan.contains("kept=[cust_apac]"),
        "expected kept=[cust_apac], got: {plan}"
    );
}

#[test]
fn explain_lists_default_only_when_no_concrete_match() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE cust (id BIGINT, region TEXT) PARTITION BY LIST (region)")
        .unwrap();
    e.execute("CREATE TABLE cust_apac PARTITION OF cust FOR VALUES IN ('jp')")
        .unwrap();
    e.execute("CREATE TABLE cust_default PARTITION OF cust DEFAULT")
        .unwrap();

    // region = 'us' isn't in any concrete LIST → only DEFAULT
    // survives.
    let plan = explain_text(&mut e, "EXPLAIN SELECT * FROM cust WHERE region = 'us'");
    assert!(
        plan.contains("kept=[cust_default]"),
        "expected kept=[cust_default]: {plan}"
    );
    assert!(
        !plan.contains("cust_apac"),
        "cust_apac should be pruned: {plan}"
    );
}

#[test]
fn explain_lists_all_children_when_no_predicate() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE cust (id BIGINT, region TEXT) PARTITION BY LIST (region)")
        .unwrap();
    e.execute("CREATE TABLE cust_apac PARTITION OF cust FOR VALUES IN ('jp')")
        .unwrap();
    e.execute("CREATE TABLE cust_emea PARTITION OF cust FOR VALUES IN ('de')")
        .unwrap();
    e.execute("CREATE TABLE cust_default PARTITION OF cust DEFAULT")
        .unwrap();

    let plan = explain_text(&mut e, "EXPLAIN SELECT * FROM cust");
    // No equality literal → every concrete + DEFAULT child kept.
    assert!(plan.contains("cust_apac"), "missing cust_apac: {plan}");
    assert!(plan.contains("cust_emea"), "missing cust_emea: {plan}");
    assert!(
        plan.contains("cust_default"),
        "missing cust_default: {plan}"
    );
}

#[test]
fn explain_range_keeps_overlapping_children() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE events (id BIGINT, received_at TIMESTAMPTZ) PARTITION BY RANGE (received_at)",
    )
    .unwrap();
    e.execute(
        "CREATE TABLE events_2026_06 PARTITION OF events \
         FOR VALUES FROM ('2026-06-01 00:00:00+00') TO ('2026-07-01 00:00:00+00')",
    )
    .unwrap();
    e.execute(
        "CREATE TABLE events_2026_07 PARTITION OF events \
         FOR VALUES FROM ('2026-07-01 00:00:00+00') TO ('2026-08-01 00:00:00+00')",
    )
    .unwrap();

    let plan = explain_text(
        &mut e,
        "EXPLAIN SELECT * FROM events \
         WHERE received_at >= '2026-07-01 00:00:00+00'",
    );
    assert!(
        plan.contains("kept=[events_2026_07]"),
        "expected kept=[events_2026_07], got: {plan}"
    );
    assert!(
        !plan.contains("events_2026_06"),
        "events_2026_06 should be pruned: {plan}"
    );
}

// v7.37.16 (16.12) — PG partition catalog scalar functions.

fn one_text(e: &mut Engine, sql: &str) -> Option<String> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    match &rows[0].values[0] {
        Value::Text(s) => Some(s.to_string()),
        Value::Null => None,
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn pg_partition_root_walks_to_top_ancestor() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE cust (id BIGINT, region TEXT) PARTITION BY LIST (region)")
        .unwrap();
    e.execute("CREATE TABLE cust_apac PARTITION OF cust FOR VALUES IN ('jp')")
        .unwrap();

    // Child's root walks up.
    assert_eq!(
        one_text(&mut e, "SELECT pg_partition_root('cust_apac')"),
        Some("cust".to_string())
    );
    // Parent's root is itself.
    assert_eq!(
        one_text(&mut e, "SELECT pg_partition_root('cust')"),
        Some("cust".to_string())
    );
    // Non-existent: NULL.
    assert_eq!(
        one_text(&mut e, "SELECT pg_partition_root('does_not_exist')"),
        None
    );
    // Plain table is its own root.
    e.execute("CREATE TABLE plain (id BIGINT)").unwrap();
    assert_eq!(
        one_text(&mut e, "SELECT pg_partition_root('plain')"),
        Some("plain".to_string())
    );
}

#[test]
fn pg_partition_ancestors_returns_leaf_to_root_chain() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE cust (id BIGINT, region TEXT) PARTITION BY LIST (region)")
        .unwrap();
    e.execute("CREATE TABLE cust_apac PARTITION OF cust FOR VALUES IN ('jp')")
        .unwrap();

    // Leaf → root for a partition child.
    assert_eq!(
        one_text(&mut e, "SELECT pg_partition_ancestors('cust_apac')"),
        Some("cust_apac,cust".to_string())
    );
    // Parent returns just itself.
    assert_eq!(
        one_text(&mut e, "SELECT pg_partition_ancestors('cust')"),
        Some("cust".to_string())
    );
    // NULL input → NULL.
    assert_eq!(
        one_text(&mut e, "SELECT pg_partition_ancestors(NULL)"),
        None
    );
}

#[test]
fn explain_hash_keeps_only_residue_class_child() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE oh (id BIGINT) PARTITION BY HASH (id)")
        .unwrap();
    for r in 0..4 {
        let sql = format!(
            "CREATE TABLE oh_{r} PARTITION OF oh FOR VALUES WITH (MODULUS 4, REMAINDER {r})"
        );
        e.execute(&sql).unwrap();
    }
    // Pick the bucket id=42 actually hashes to + check EXPLAIN
    // surfaces ONLY that one.
    e.execute("INSERT INTO oh VALUES (42)").unwrap();
    let mut owner = None;
    for r in 0..4 {
        let sql = format!("SELECT COUNT(*) FROM oh_{r}");
        let rs = rows(&mut e, &sql);
        let Value::BigInt(n) = rs[0][0] else { panic!() };
        if n == 1 {
            owner = Some(r);
            break;
        }
    }
    let owner = owner.expect("some bucket owns id=42");

    let plan = explain_text(&mut e, "EXPLAIN SELECT * FROM oh WHERE id = 42");
    let kept_marker = format!("kept=[oh_{owner}]");
    assert!(
        plan.contains(&kept_marker),
        "expected {kept_marker}, got: {plan}"
    );
}
