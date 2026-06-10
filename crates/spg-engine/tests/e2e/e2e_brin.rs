//! v6.7.1 — BRIN-style segment-level sidecar.
//!
//! Ships the FORMAT layer: CREATE INDEX … USING BRIN syntax,
//! storage-side IndexKind::Brin variant, segment v2 envelope
//! sidecar emit/parse round-trip. Planner integration
//! (page-skipping during scan) is carved out: SPG's cold-tier
//! access is locator-based not scan-based, so per-page skipping
//! during full scan doesn't fit the current architecture. The
//! summaries are stored for future use by v6.7.3 compaction and
//! any later columnar-cold-tier evolution.

use spg_engine::{Engine, QueryResult};

fn rows_of(res: QueryResult) -> Vec<Vec<spg_storage::Value>> {
    match res {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn create_index_brin_succeeds() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    // Parser + dispatch path.
    eng.execute("CREATE INDEX ix_t_brin ON t USING brin (id)")
        .unwrap();
    // Idempotency via IF NOT EXISTS.
    eng.execute("CREATE INDEX IF NOT EXISTS ix_t_brin ON t USING brin (id)")
        .unwrap();
    // Duplicate without IF NOT EXISTS errors.
    let err = eng.execute("CREATE INDEX ix_t_brin ON t USING brin (id)");
    assert!(
        err.is_err(),
        "expected DuplicateIndex without IF NOT EXISTS"
    );
}

#[test]
fn brin_coexists_with_btree_on_same_column() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    eng.execute("CREATE INDEX ix_t_btree ON t (id)").unwrap();
    eng.execute("CREATE INDEX ix_t_brin ON t USING brin (id)")
        .unwrap();
    // Insert + select still works.
    for i in 0..5 {
        eng.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
    }
    let res = rows_of(eng.execute("SELECT id FROM t").unwrap());
    assert_eq!(res.len(), 5);
}

#[test]
fn brin_index_does_not_break_full_table_scan() {
    // BRIN doesn't participate in lookups directly (it's metadata
    // for cold segments). A SELECT on a table with only a BRIN
    // index should still work via the regular row scan.
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    eng.execute("CREATE INDEX ix_t_brin ON t USING brin (id)")
        .unwrap();
    eng.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
    eng.execute("INSERT INTO t VALUES (2, 'b')").unwrap();
    eng.execute("INSERT INTO t VALUES (3, 'c')").unwrap();
    let res = rows_of(eng.execute("SELECT id, name FROM t WHERE id = 2").unwrap());
    assert_eq!(res.len(), 1);
}

#[test]
fn brin_summaries_get_emitted_at_freeze_when_brin_index_exists() {
    // This test exercises the v6.7.1 wire: the freezer's
    // persist_segment path will (in v6.7.x follow-up) consult
    // Table.indices for any BRIN index on the freeze column and
    // call wrap_v2_envelope_with_brin. For now (v6.7.1 wire only),
    // we drive the encode + wrap directly and verify the segment
    // round-trips with summaries intact.

    use spg_storage::{
        Catalog, ColumnSchema, DataType, OwnedSegment, SEGMENT_PAGE_BYTES, TableSchema,
        derive_brin_summaries, encode_segment, wrap_v2_envelope_with_brin,
    };

    let _cat: Catalog = Catalog::new();
    let _schema = TableSchema {
        name: "t".to_string(),
        columns: vec![ColumnSchema::new("id".to_string(), DataType::Int, false)],
        // v6.7.2 added per-table hot_tier_bytes override; defaults
        // to None (= follow global SPG_HOT_TIER_BYTES).
        hot_tier_bytes: None,
        foreign_keys: Vec::new(),
        uniqueness_constraints: Vec::new(),
        checks: Vec::new(),
    };
    // Encode 100 rows with sparse keys.
    let pairs: Vec<(u64, Vec<u8>)> = (0..100u64)
        .map(|i| (i * 3 + 1, vec![b'r', b'-', i as u8]))
        .collect();
    let (v1, meta) = encode_segment(pairs.into_iter(), 0.01, SEGMENT_PAGE_BYTES).expect("encode");
    let summaries = derive_brin_summaries(&v1).expect("derive");
    assert_eq!(summaries.len(), meta.num_pages as usize);

    let wrapped = wrap_v2_envelope_with_brin(v1, &summaries, true);
    let seg = OwnedSegment::from_bytes(wrapped).expect("v2+brin parses");
    assert_eq!(seg.brin_summaries().len(), summaries.len());
    // Lookups still work after the BRIN sidecar round-trip.
    assert!(seg.lookup(1).is_some());
    assert!(seg.lookup(298).is_some());
}
