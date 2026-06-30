//! v7.37.16 (16.3 / 16.4 / 16.5) — ATTACH PARTITION + DETACH
//! PARTITION acceptance suite.

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

fn one_i64(e: &mut Engine, sql: &str) -> i64 {
    let mut rs = rows(e, sql);
    let row = rs.pop().expect("one row");
    match row.into_iter().next().expect("one col") {
        Value::BigInt(n) => n,
        Value::Int(n) => i64::from(n),
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn attach_range_partition_round_trips() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE events_p (
             id BIGINT NOT NULL,
             received_at TIMESTAMPTZ NOT NULL
         ) PARTITION BY RANGE (received_at)",
    )
    .unwrap();
    // Standalone child with parent-matching layout.
    e.execute(
        "CREATE TABLE events_2026_06 (id BIGINT NOT NULL, received_at TIMESTAMPTZ NOT NULL)",
    )
    .unwrap();
    // Attach it.
    e.execute(
        "ALTER TABLE events_p ATTACH PARTITION events_2026_06 \
         FOR VALUES FROM ('2026-06-01 00:00:00+00') TO ('2026-07-01 00:00:00+00')",
    )
    .unwrap();
    // Parent INSERT now routes through.
    e.execute(
        "INSERT INTO events_p VALUES (1, '2026-06-15 12:00:00+00')",
    )
    .unwrap();
    assert_eq!(
        one_i64(&mut e, "SELECT COUNT(*) FROM events_2026_06"),
        1
    );
    assert_eq!(one_i64(&mut e, "SELECT COUNT(*) FROM events_p"), 1);
}

#[test]
fn attach_list_partition_routes_existing_inserts() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE c_l (id BIGINT, region TEXT) PARTITION BY LIST (region)",
    )
    .unwrap();
    e.execute("CREATE TABLE c_apac (id BIGINT, region TEXT)")
        .unwrap();
    e.execute(
        "ALTER TABLE c_l ATTACH PARTITION c_apac FOR VALUES IN ('jp', 'kr')",
    )
    .unwrap();
    e.execute("INSERT INTO c_l VALUES (1, 'jp')").unwrap();
    assert_eq!(one_i64(&mut e, "SELECT COUNT(*) FROM c_apac"), 1);
}

#[test]
fn attach_hash_partition_routes_existing_inserts() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE oh (id BIGINT) PARTITION BY HASH (id)")
        .unwrap();
    e.execute("CREATE TABLE oh_0 (id BIGINT)").unwrap();
    e.execute("CREATE TABLE oh_1 (id BIGINT)").unwrap();
    e.execute(
        "ALTER TABLE oh ATTACH PARTITION oh_0 FOR VALUES WITH (MODULUS 2, REMAINDER 0)",
    )
    .unwrap();
    e.execute(
        "ALTER TABLE oh ATTACH PARTITION oh_1 FOR VALUES WITH (MODULUS 2, REMAINDER 1)",
    )
    .unwrap();
    for i in 0..10 {
        let sql = format!("INSERT INTO oh VALUES ({i})");
        e.execute(&sql).unwrap();
    }
    let c0 = one_i64(&mut e, "SELECT COUNT(*) FROM oh_0");
    let c1 = one_i64(&mut e, "SELECT COUNT(*) FROM oh_1");
    assert_eq!(c0 + c1, 10);
}

#[test]
fn attach_rejects_layout_mismatch() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE p (id BIGINT, received_at TIMESTAMPTZ) PARTITION BY RANGE (received_at)",
    )
    .unwrap();
    // Different column name.
    e.execute("CREATE TABLE c_wrong (xid BIGINT, received_at TIMESTAMPTZ)")
        .unwrap();
    let err = e
        .execute(
            "ALTER TABLE p ATTACH PARTITION c_wrong \
             FOR VALUES FROM ('2026-01-01 00:00:00+00') TO ('2027-01-01 00:00:00+00')",
        )
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("doesn't match") || msg.contains("column"),
        "expected layout-mismatch error: {msg}"
    );
}

#[test]
fn attach_rejects_non_empty_child() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE p (id BIGINT, region TEXT) PARTITION BY LIST (region)",
    )
    .unwrap();
    e.execute("CREATE TABLE c (id BIGINT, region TEXT)").unwrap();
    e.execute("INSERT INTO c VALUES (1, 'jp')").unwrap();
    let err = e
        .execute("ALTER TABLE p ATTACH PARTITION c FOR VALUES IN ('jp')")
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("existing rows") || msg.contains("empty"),
        "expected non-empty-child rejection: {msg}"
    );
}

#[test]
fn attach_rejects_overlap_with_existing_sibling() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE p (id BIGINT, received_at TIMESTAMPTZ) PARTITION BY RANGE (received_at)",
    )
    .unwrap();
    e.execute(
        "CREATE TABLE c1 PARTITION OF p \
         FOR VALUES FROM ('2026-06-01 00:00:00+00') TO ('2026-07-01 00:00:00+00')",
    )
    .unwrap();
    e.execute("CREATE TABLE c2 (id BIGINT, received_at TIMESTAMPTZ)").unwrap();
    let err = e
        .execute(
            "ALTER TABLE p ATTACH PARTITION c2 \
             FOR VALUES FROM ('2026-06-15 00:00:00+00') TO ('2026-08-01 00:00:00+00')",
        )
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("overlaps"),
        "expected overlap rejection: {msg}"
    );
}

#[test]
fn detach_partition_makes_child_standalone() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE c_l (id BIGINT, region TEXT) PARTITION BY LIST (region)",
    )
    .unwrap();
    e.execute("CREATE TABLE c_apac PARTITION OF c_l FOR VALUES IN ('jp')")
        .unwrap();
    e.execute("INSERT INTO c_l VALUES (1, 'jp')").unwrap();
    assert_eq!(one_i64(&mut e, "SELECT COUNT(*) FROM c_apac"), 1);

    // Detach.
    e.execute("ALTER TABLE c_l DETACH PARTITION c_apac").unwrap();

    // c_apac retains its rows but is no longer in the parent UNION.
    assert_eq!(one_i64(&mut e, "SELECT COUNT(*) FROM c_apac"), 1);
    assert_eq!(one_i64(&mut e, "SELECT COUNT(*) FROM c_l"), 0);

    // Insert into c_apac directly works (standalone).
    e.execute("INSERT INTO c_apac VALUES (2, 'jp')").unwrap();
    assert_eq!(one_i64(&mut e, "SELECT COUNT(*) FROM c_apac"), 2);
}

#[test]
fn detach_concurrently_accepted_at_parser() {
    // CONCURRENTLY in SPG single-engine is semantically identical
    // to the regular DETACH — the parser accepts it; the engine
    // performs the same atomic detach (PG's two-phase split exists
    // to address replication lag, which doesn't apply here).
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE c_l (id BIGINT, region TEXT) PARTITION BY LIST (region)",
    )
    .unwrap();
    e.execute("CREATE TABLE c_apac PARTITION OF c_l FOR VALUES IN ('jp')")
        .unwrap();
    e.execute("ALTER TABLE c_l DETACH PARTITION c_apac CONCURRENTLY")
        .unwrap();
    e.execute("ALTER TABLE c_l ATTACH PARTITION c_apac FOR VALUES IN ('jp')")
        .unwrap();
    e.execute("ALTER TABLE c_l DETACH PARTITION c_apac CONCURRENTLY FINALIZE")
        .unwrap();
}

#[test]
fn detach_rejects_non_child() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p (id BIGINT, region TEXT) PARTITION BY LIST (region)")
        .unwrap();
    e.execute("CREATE TABLE not_a_child (id BIGINT, region TEXT)")
        .unwrap();
    let err = e
        .execute("ALTER TABLE p DETACH PARTITION not_a_child")
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("not a partition") || msg.contains("not_a_child"),
        "expected non-partition rejection: {msg}"
    );
}

#[test]
fn attach_then_detach_round_trip_preserves_standalone_inserts() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE p (id BIGINT, region TEXT) PARTITION BY LIST (region)",
    )
    .unwrap();
    e.execute("CREATE TABLE c (id BIGINT, region TEXT)").unwrap();
    e.execute("ALTER TABLE p ATTACH PARTITION c FOR VALUES IN ('jp')")
        .unwrap();
    e.execute("INSERT INTO p VALUES (1, 'jp')").unwrap();
    e.execute("ALTER TABLE p DETACH PARTITION c").unwrap();
    // Round-trip back through ATTACH — c has rows so this should
    // be rejected (consistent with the 16.3 empty-child gate).
    let err = e
        .execute("ALTER TABLE p ATTACH PARTITION c FOR VALUES IN ('jp')")
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("existing rows") || msg.contains("empty"),
        "expected non-empty-child rejection on re-attach: {msg}"
    );
}
