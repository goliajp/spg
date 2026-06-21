//! v6.7.0 — per-table cold_rows precise count.
//!
//! Resolves the v6.2.7 STABILITY carve-out: spg_statistic gains a
//! cold_row_count column populated by ANALYZE; spg_stat_segment
//! gains a table_name column populated by walking BTree-index Cold
//! locators.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows_of(res: QueryResult) -> Vec<Vec<Value<'static>>> {
    match res {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

fn columns_of(res: QueryResult) -> Vec<String> {
    match res {
        QueryResult::Rows { columns, .. } => columns.into_iter().map(|c| c.name).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn spg_statistic_has_cold_row_count_column() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    let cols = columns_of(eng.execute("SELECT * FROM spg_statistic").unwrap());
    assert_eq!(
        cols,
        vec![
            "table_name".to_string(),
            "column_name".to_string(),
            "null_frac".to_string(),
            "n_distinct".to_string(),
            "histogram_bounds".to_string(),
            "cold_row_count".to_string(),
        ]
    );
}

#[test]
fn spg_stat_segment_has_table_name_column() {
    let mut eng = Engine::new();
    let cols = columns_of(eng.execute("SELECT * FROM spg_stat_segment").unwrap());
    assert_eq!(
        cols,
        vec![
            "segment_id".to_string(),
            "table_name".to_string(),
            "num_rows".to_string(),
            "num_pages".to_string(),
            "total_bytes".to_string(),
        ]
    );
}

#[test]
fn analyze_populates_cold_row_count_after_freeze() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    eng.execute("CREATE INDEX ix_t_id ON t (id)").unwrap();
    // Insert 20 rows.
    for i in 0..20 {
        eng.execute(&format!("INSERT INTO t VALUES ({i}, 'r{i}')"))
            .unwrap();
    }
    // Freeze the oldest 8 rows into a cold segment.
    let report = eng
        .freeze_oldest_to_cold("t", "ix_t_id", 8)
        .expect("freeze");
    assert_eq!(report.frozen_rows, 8);
    // Before ANALYZE, the cached count is stale (we marked it so).
    // The spg_statistic surface returns whatever is cached; the
    // stale flag is internal. ANALYZE refreshes.
    eng.execute("ANALYZE t").unwrap();
    let stats = rows_of(eng.execute("SELECT * FROM spg_statistic").unwrap());
    // At least one row per non-vector column; cold_row_count is
    // column-index 5.
    assert!(!stats.is_empty(), "ANALYZE produced spg_statistic rows");
    for row in &stats {
        assert_eq!(
            row[5],
            Value::BigInt(8),
            "expected cold_row_count = 8 across every column row"
        );
    }
}

#[test]
fn spg_stat_segment_resolves_table_name() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    eng.execute("CREATE INDEX ix_t ON t (id)").unwrap();
    for i in 0..10 {
        eng.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
    }
    let _ = eng.freeze_oldest_to_cold("t", "ix_t", 5).expect("freeze");

    let rows = rows_of(eng.execute("SELECT * FROM spg_stat_segment").unwrap());
    assert!(!rows.is_empty(), "expected at least one segment row");
    // table_name is column 1; should match "t" for every segment
    // that holds cold rows owned by t.
    for row in &rows {
        assert_eq!(
            row[1],
            Value::text("t"),
            "expected table_name='t' for segment owned by t"
        );
    }
}

/// v7.34.6 (mailrs prod #6) — the JOIN walker's hot/cold tier
/// dispatch. Pre-7.34.6 the walker bailed entirely on the first
/// cold-tier RowLocator it saw, which was the prod 803 MB silent
/// fall-back to the 82 ms NOT-IN scan-and-sort. This test mimics
/// the `content_worker` shape (`messages JOIN mailboxes ... ORDER
/// BY m.id DESC LIMIT N`) on a table whose oldest rows have been
/// promoted to cold; the walker MUST still return the correct N
/// rows by walking the index across both tiers.
#[test]
fn join_walker_spans_hot_cold_tier_on_order_by_limit() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE mailboxes (id BIGINT NOT NULL PRIMARY KEY, name TEXT NOT NULL)")
        .unwrap();
    eng.execute(
        "CREATE TABLE messages (id BIGINT NOT NULL PRIMARY KEY, mailbox_id BIGINT NOT NULL, \
            size BIGINT NOT NULL)",
    )
    .unwrap();
    for i in 0..4 {
        eng.execute(&format!("INSERT INTO mailboxes VALUES ({i}, 'mb{i}')"))
            .unwrap();
    }
    for i in 1..=100 {
        eng.execute(&format!(
            "INSERT INTO messages VALUES ({i}, {}, {})",
            i % 4,
            if i % 17 == 0 { 0 } else { 1024 }
        ))
        .unwrap();
    }
    // Baseline result before any freeze — what the walker SHOULD
    // return after we promote half the rows to cold.
    let sql = "SELECT m.id, mb.name FROM messages m \
               JOIN mailboxes mb ON m.mailbox_id = mb.id \
               WHERE m.size > 0 \
               ORDER BY m.id DESC LIMIT 10";
    let baseline = rows_of(eng.execute(sql).unwrap());
    assert_eq!(baseline.len(), 10);
    // The newest 10 rows that pass size > 0 — none of m.id ∈ {17, 34,
    // 51, 68, 85} since those are size=0, so expected = 100, 99, 98, 97,
    // 96, 95, 94, 93, 92, 91.
    let ids: Vec<i64> = baseline
        .iter()
        .map(|r| match r[0] {
            Value::BigInt(n) => n,
            _ => panic!("expected BigInt id"),
        })
        .collect();
    assert_eq!(ids, vec![100, 99, 98, 97, 96, 95, 94, 93, 92, 91]);
    // Freeze the oldest 50 rows to cold.
    let report = eng
        .freeze_oldest_to_cold("messages", "messages_pkey", 50)
        .expect("freeze cold");
    assert_eq!(report.frozen_rows, 50);
    // The walker MUST still return the same 10 rows. The hot half
    // holds 51..=100, so the first 10 keys touched by ORDER BY id
    // DESC are all hot — no cold dispatch needed. This case just
    // verifies the walker keeps working when cold segments exist
    // for the table (the freeze itself doesn't re-route the plan).
    let after_freeze_top = rows_of(eng.execute(sql).unwrap());
    let ids_top: Vec<i64> = after_freeze_top
        .iter()
        .map(|r| match r[0] {
            Value::BigInt(n) => n,
            _ => panic!("expected BigInt id"),
        })
        .collect();
    assert_eq!(ids_top, vec![100, 99, 98, 97, 96, 95, 94, 93, 92, 91]);
    // Now request the OLDEST 10 surviving rows — the walker has to
    // walk ASC across hot+cold, with the first hits coming from the
    // cold segment (ids 1..=50, minus those with size=0). Expected =
    // 1, 2, 3, ..., 10 (none of those have size=0).
    let asc_sql = "SELECT m.id, mb.name FROM messages m \
                   JOIN mailboxes mb ON m.mailbox_id = mb.id \
                   WHERE m.size > 0 \
                   ORDER BY m.id ASC LIMIT 10";
    let asc = rows_of(eng.execute(asc_sql).unwrap());
    let ids_asc: Vec<i64> = asc
        .iter()
        .map(|r| match r[0] {
            Value::BigInt(n) => n,
            _ => panic!("expected BigInt id"),
        })
        .collect();
    assert_eq!(
        ids_asc,
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
        "ASC walker must traverse cold-tier rows correctly"
    );
}

/// v7.34.7 (mailrs prod #6 follow-up) — single-table walker's
/// hot/cold dispatch (the JOIN walker's counterpart from
/// `join_walker_spans_hot_cold_tier_on_order_by_limit`). The
/// `try_pk_walk_top_n` path drives `SELECT … FROM t WHERE …
/// ORDER BY <indexed col> LIMIT N` shapes — `mailrs_prod_plain_limit`
/// being the load-bearing prod example. Pre-7.34.7 bailed on the
/// first cold-tier RowLocator (`Vec<usize>` return ties it to hot
/// row indices); 7.34.7 returns `Vec<Cow<Row>>` and resolves cold
/// locators through `Catalog::resolve_cold_locator` inline.
#[test]
fn single_table_walker_spans_hot_cold_tier_on_order_by_limit() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id BIGINT NOT NULL PRIMARY KEY, payload TEXT NOT NULL)")
        .unwrap();
    for i in 1..=100 {
        eng.execute(&format!("INSERT INTO t VALUES ({i}, 'row-{i}')"))
            .unwrap();
    }
    // Freeze the oldest 50 to cold; the newest 50 stay hot.
    let report = eng
        .freeze_oldest_to_cold("t", "t_pkey", 50)
        .expect("freeze cold");
    assert_eq!(report.frozen_rows, 50);
    // ASC LIMIT 10 must walk cold locators inline and return rows 1..=10.
    let asc = rows_of(
        eng.execute("SELECT id FROM t WHERE id > 0 ORDER BY id ASC LIMIT 10")
            .unwrap(),
    );
    let ids_asc: Vec<i64> = asc
        .iter()
        .map(|r| match r[0] {
            Value::BigInt(n) => n,
            _ => panic!("expected BigInt id"),
        })
        .collect();
    assert_eq!(
        ids_asc,
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
        "single-table ASC walker must traverse cold-tier rows"
    );
    // DESC LIMIT 10 stays in the hot tier (newest 50 are hot).
    let desc = rows_of(
        eng.execute("SELECT id FROM t WHERE id > 0 ORDER BY id DESC LIMIT 10")
            .unwrap(),
    );
    let ids_desc: Vec<i64> = desc
        .iter()
        .map(|r| match r[0] {
            Value::BigInt(n) => n,
            _ => panic!("expected BigInt id"),
        })
        .collect();
    assert_eq!(ids_desc, vec![100, 99, 98, 97, 96, 95, 94, 93, 92, 91]);
}

/// v7.35.1 — `materialise_table_ref_filtered` cold-tier dispatch
/// for the peer table side of a JOIN. Pre-7.35.1 the helper only
/// walked `Table::rows()` (= hot tier), so any cold-tier row on
/// the peer side was silently dropped from the join result —
/// queries returned wrong row counts without erroring. Test seeds
/// 100 mailboxes + 100 messages (1:1 on `mailbox_id`), freezes
/// the oldest 50 mailboxes to a cold segment, then runs a
/// `messages JOIN mailboxes` count + value check. The full join
/// MUST still resolve every survivor.
#[test]
fn peer_table_cold_tier_rows_visible_to_join() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE mailboxes (id BIGINT NOT NULL PRIMARY KEY, name TEXT NOT NULL)")
        .unwrap();
    eng.execute(
        "CREATE TABLE messages (id BIGINT NOT NULL PRIMARY KEY, mailbox_id BIGINT NOT NULL)",
    )
    .unwrap();
    for i in 1..=100 {
        eng.execute(&format!("INSERT INTO mailboxes VALUES ({i}, 'mb-{i}')"))
            .unwrap();
        eng.execute(&format!("INSERT INTO messages VALUES ({i}, {i})"))
            .unwrap();
    }
    // Freeze the oldest 50 mailboxes to cold; mailbox ids 1..=50
    // now live in a cold segment, ids 51..=100 stay hot.
    let report = eng
        .freeze_oldest_to_cold("mailboxes", "mailboxes_pkey", 50)
        .expect("freeze cold");
    assert_eq!(report.frozen_rows, 50);
    // Sanity 1: SELECT * FROM mailboxes — must see all 100 rows
    // (50 hot + 50 cold). If this fails, the cold-tier lift never
    // surfaces, and the join below has no chance.
    let mb_all = rows_of(eng.execute("SELECT id FROM mailboxes").unwrap());
    assert_eq!(mb_all.len(), 100, "SELECT * lost cold-tier rows");
    // COUNT(*) — every message has a mailbox; the join survivor
    // count MUST be 100 even though half the mailboxes are cold.
    let rows = rows_of(
        eng.execute("SELECT COUNT(*) FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id")
            .unwrap(),
    );
    assert_eq!(
        rows[0][0],
        Value::BigInt(100),
        "join lost cold-tier peer rows"
    );
    // Value check: pick a known cold-tier id (mailbox 5) and confirm
    // its name flows through the join. Pre-7.35.1 this projection
    // returned 0 rows because mb.name was unreachable.
    let by_id = rows_of(
        eng.execute(
            "SELECT mb.name FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id \
             WHERE m.id = 5",
        )
        .unwrap(),
    );
    assert_eq!(by_id.len(), 1);
    assert_eq!(by_id[0][0], Value::text("mb-5"));
}

/// v7.36 — primary-table cold-tier coverage for the legacy
/// `try_streamed_inner_join_topn` path (`ORDER BY <non-indexed col>
/// LIMIT N`). The 7.34.5 walker only handled `ORDER BY <indexed
/// col>`; this older streamed-topN path was hot-only for the
/// primary scan. Now both tiers are chained.
#[test]
fn streamed_topn_join_primary_spans_hot_cold_tier() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE mailboxes (id BIGINT NOT NULL PRIMARY KEY, name TEXT NOT NULL)")
        .unwrap();
    eng.execute(
        "CREATE TABLE messages (id BIGINT NOT NULL PRIMARY KEY, mailbox_id BIGINT NOT NULL, \
            score BIGINT NOT NULL)",
    )
    .unwrap();
    for i in 1..=4 {
        eng.execute(&format!("INSERT INTO mailboxes VALUES ({i}, 'mb-{i}')"))
            .unwrap();
    }
    for i in 1..=100 {
        eng.execute(&format!(
            "INSERT INTO messages VALUES ({i}, {}, {})",
            (i % 4) + 1,
            i
        ))
        .unwrap();
    }
    // Freeze the oldest 50 messages — ids 1..=50 cold, 51..=100 hot.
    let report = eng
        .freeze_oldest_to_cold("messages", "messages_pkey", 50)
        .expect("freeze cold");
    assert_eq!(report.frozen_rows, 50);
    // Sanity 1: COUNT(*) without JOIN exercises the single-table
    // cold loop. Must see all 100 rows.
    let cnt = rows_of(eng.execute("SELECT COUNT(*) FROM messages").unwrap());
    assert_eq!(
        cnt[0][0],
        Value::BigInt(100),
        "SELECT COUNT(*) lost cold-tier rows"
    );
    // Sanity 2: bare SELECT — same path.
    let all = rows_of(eng.execute("SELECT id FROM messages WHERE id > 0").unwrap());
    assert_eq!(all.len(), 100, "bare SELECT lost cold-tier rows");
    // ORDER BY score ASC LIMIT 10 — `score` has no index, so the
    // walker shape doesn't match and we fall through to the legacy
    // streamed-topN heap. The lowest 10 scores are 1..=10, all in
    // the cold tier — pre-7.36 this returned the lowest 10 HOT
    // scores instead (51..=60).
    let rows = rows_of(
        eng.execute(
            "SELECT m.id, m.score FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id \
             ORDER BY m.score ASC LIMIT 10",
        )
        .unwrap(),
    );
    let scores: Vec<i64> = rows
        .iter()
        .map(|r| match r[1] {
            Value::BigInt(n) => n,
            _ => panic!("expected BigInt score"),
        })
        .collect();
    assert_eq!(
        scores,
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
        "streamed-topN primary scan must traverse cold-tier rows"
    );
}

/// v7.36 — composite FK validation must see cold-tier parent rows.
/// Pre-7.36 `enforce_fk_inserts` walked `parent.rows().iter()` only,
/// so a child INSERT whose matching parent had been frozen to cold
/// raised `FOREIGN KEY violation: no parent row` falsely.
#[test]
fn composite_fk_check_sees_cold_parent_rows() {
    let mut eng = Engine::new();
    eng.execute(
        "CREATE TABLE parent (\
            tenant_id BIGINT NOT NULL, \
            entity_id BIGINT NOT NULL, \
            name TEXT NOT NULL, \
            id BIGINT NOT NULL PRIMARY KEY, \
            UNIQUE(tenant_id, entity_id))",
    )
    .unwrap();
    eng.execute(
        "CREATE TABLE child (\
            id BIGINT NOT NULL PRIMARY KEY, \
            tenant_id BIGINT NOT NULL, \
            entity_id BIGINT NOT NULL, \
            FOREIGN KEY (tenant_id, entity_id) REFERENCES parent (tenant_id, entity_id))",
    )
    .unwrap();
    for i in 1..=10 {
        eng.execute(&format!("INSERT INTO parent VALUES (1, {i}, 'p-{i}', {i})"))
            .unwrap();
    }
    // Freeze the oldest 5 parents — ids 1..=5 cold, 6..=10 hot.
    let report = eng
        .freeze_oldest_to_cold("parent", "parent_pkey", 5)
        .expect("freeze parent");
    assert_eq!(report.frozen_rows, 5);
    // INSERT into child referencing a HOT parent should pass.
    eng.execute("INSERT INTO child VALUES (100, 1, 7)")
        .expect("hot parent reference must pass");
    // INSERT into child referencing a COLD parent — pre-7.36 raised
    // FOREIGN KEY violation. Post-7.36 must pass.
    eng.execute("INSERT INTO child VALUES (101, 1, 2)")
        .expect("cold parent reference must pass");
    // INSERT into child referencing a missing parent must still
    // raise; cold visibility doesn't paper over a genuine miss.
    let res = eng.execute("INSERT INTO child VALUES (102, 1, 999)");
    assert!(res.is_err(), "missing parent reference must still error");
}

/// v7.36 — composite ON CONFLICT key existence check must see
/// cold-tier rows. Pre-7.36 a `(a, b)` UNIQUE conflict whose
/// existing row was cold went undetected, so ON CONFLICT DO
/// NOTHING wrote a duplicate. Test seeds 10 rows under a composite
/// UNIQUE, freezes the oldest 5, then INSERT … ON CONFLICT DO
/// NOTHING against a cold-tier conflict key. The INSERT must be
/// skipped (no duplicate row written).
#[test]
fn composite_on_conflict_key_check_sees_cold_rows() {
    let mut eng = Engine::new();
    eng.execute(
        "CREATE TABLE t (\
            id BIGINT NOT NULL PRIMARY KEY, \
            tenant_id BIGINT NOT NULL, \
            entity_id BIGINT NOT NULL, \
            UNIQUE(tenant_id, entity_id))",
    )
    .unwrap();
    for i in 1..=10 {
        eng.execute(&format!("INSERT INTO t VALUES ({i}, 1, {i})"))
            .unwrap();
    }
    let report = eng.freeze_oldest_to_cold("t", "t_pkey", 5).expect("freeze");
    assert_eq!(report.frozen_rows, 5);
    let before_cnt = match eng.execute("SELECT COUNT(*) FROM t").unwrap() {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        _ => panic!("expected Rows"),
    };
    assert_eq!(before_cnt, Value::BigInt(10));
    // Conflict against cold (tenant_id=1, entity_id=2) — must be
    // detected, ON CONFLICT DO NOTHING short-circuits.
    eng.execute("INSERT INTO t VALUES (100, 1, 2) ON CONFLICT (tenant_id, entity_id) DO NOTHING")
        .expect("ON CONFLICT DO NOTHING must not error");
    let after_cnt = match eng.execute("SELECT COUNT(*) FROM t").unwrap() {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        _ => panic!("expected Rows"),
    };
    assert_eq!(
        after_cnt,
        Value::BigInt(10),
        "ON CONFLICT DO NOTHING must skip when conflict is cold"
    );
}

/// v7.36 — DELETE cascade FK planner must surface cold-tier child
/// references explicitly instead of silently skipping them. Pre-7.36
/// the cold child was invisible to RESTRICT (lost integrity check)
/// and to Cascade/SetNull/SetDefault (orphaned cold child). PG and
/// MariaDB never let an FK violation become silent — we keep that
/// invariant by erroring with a clear "cold-tier mutation" message
/// pointing at COMPACT / promote-then-retry as the operator path.
#[test]
fn delete_cascade_with_cold_child_surfaces_explicit_error() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE parent (id BIGINT NOT NULL PRIMARY KEY)")
        .unwrap();
    eng.execute(
        "CREATE TABLE child (\
            id BIGINT NOT NULL PRIMARY KEY, \
            parent_id BIGINT NOT NULL, \
            FOREIGN KEY (parent_id) REFERENCES parent (id) ON DELETE CASCADE)",
    )
    .unwrap();
    for i in 1..=10 {
        eng.execute(&format!("INSERT INTO parent VALUES ({i})"))
            .unwrap();
    }
    for i in 1..=10 {
        eng.execute(&format!("INSERT INTO child VALUES ({i}, {i})"))
            .unwrap();
    }
    // Freeze the oldest 5 children — ids 1..=5 cold.
    let report = eng
        .freeze_oldest_to_cold("child", "child_pkey", 5)
        .expect("freeze child");
    assert_eq!(report.frozen_rows, 5);
    // DELETE a parent whose child is cold — must raise explicit
    // cold-tier error (NOT silently leave the cold child orphaned).
    let res = eng.execute("DELETE FROM parent WHERE id = 3");
    let err = res.expect_err("cold child cascade must error");
    let msg = format!("{err}");
    assert!(
        msg.contains("cold-tier"),
        "expected cold-tier error, got: {msg}"
    );
    // DELETE a parent whose child is HOT — must succeed cleanly,
    // hot cascade still works.
    eng.execute("DELETE FROM parent WHERE id = 8")
        .expect("hot child cascade must still work");
}

/// v7.36 — `build_joined_filtered_rows` primary-index path threads
/// hot row indices into `JoinSrc::Stored(t.rows())`, so a JOIN
/// whose primary had cold-tier rows dropped them silently. Forced
/// materialising fallback when cold rows exist routes through
/// `materialise_table_ref_filtered` (v7.35.1, cold-aware) and
/// recovers the full row set.
#[test]
fn join_primary_with_cold_rows_uses_materialise_fallback() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE mailboxes (id BIGINT NOT NULL PRIMARY KEY, name TEXT NOT NULL)")
        .unwrap();
    eng.execute(
        "CREATE TABLE messages (id BIGINT NOT NULL PRIMARY KEY, mailbox_id BIGINT NOT NULL)",
    )
    .unwrap();
    for i in 1..=4 {
        eng.execute(&format!("INSERT INTO mailboxes VALUES ({i}, 'mb-{i}')"))
            .unwrap();
    }
    for i in 1..=100 {
        eng.execute(&format!(
            "INSERT INTO messages VALUES ({i}, {})",
            (i % 4) + 1
        ))
        .unwrap();
    }
    let report = eng
        .freeze_oldest_to_cold("messages", "messages_pkey", 50)
        .expect("freeze messages");
    assert_eq!(report.frozen_rows, 50);
    // `SELECT m.id ... ORDER BY m.id LIMIT 200` exceeds the
    // walker's small-cap and falls through to the deferred-index
    // / materialising primary path. All 100 rows must show up.
    let rows = rows_of(
        eng.execute(
            "SELECT m.id, mb.name FROM messages m \
             JOIN mailboxes mb ON m.mailbox_id = mb.id \
             ORDER BY m.id ASC LIMIT 200",
        )
        .unwrap(),
    );
    assert_eq!(rows.len(), 100, "JOIN primary lost cold-tier rows");
    let ids: Vec<i64> = rows
        .iter()
        .map(|r| match r[0] {
            Value::BigInt(n) => n,
            _ => panic!("expected BigInt id"),
        })
        .collect();
    let mut expected = (1..=100i64).collect::<Vec<_>>();
    expected.sort();
    assert_eq!(ids, expected);
}

/// v7.36 — ALTER COLUMN TYPE can't rewrite cold-tier segments
/// in place; the storage layer would carry rows encoded against
/// the old type while the schema declares the new type. PG /
/// MariaDB never half-apply a schema change; we surface the
/// architectural gap explicitly with a clear COMPACT-then-retry
/// message instead of silently corrupting cold segments.
#[test]
fn alter_column_type_blocks_when_cold_rows_present() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id BIGINT NOT NULL PRIMARY KEY, payload TEXT NOT NULL)")
        .unwrap();
    for i in 1..=10 {
        eng.execute(&format!("INSERT INTO t VALUES ({i}, '{i}')"))
            .unwrap();
    }
    eng.freeze_oldest_to_cold("t", "t_pkey", 5).expect("freeze");
    let err = eng
        .execute("ALTER TABLE t ALTER COLUMN payload TYPE BIGINT USING CAST(payload AS BIGINT)")
        .expect_err("ALTER on cold-bearing table must error");
    let msg = format!("{err}");
    assert!(
        msg.contains("cold-tier"),
        "expected cold-tier error from ALTER, got: {msg}"
    );
}

/// v7.36 — CREATE UNIQUE INDEX must detect duplicates that live
/// in cold segments. Pre-7.36 the duplicate check walked
/// `table.rows()` only, so a cold-tier duplicate slipped through
/// and the new constraint was declared on top of stale invalid
/// data — later INSERTs surfaced phantom violations.
#[test]
fn create_unique_index_catches_cold_tier_duplicate() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id BIGINT NOT NULL PRIMARY KEY, email TEXT NOT NULL)")
        .unwrap();
    // Two rows with the same email — one ultimately cold, one hot.
    eng.execute("INSERT INTO t VALUES (1, 'dup@x')").unwrap();
    eng.execute("INSERT INTO t VALUES (2, 'unique-1@x')")
        .unwrap();
    eng.execute("INSERT INTO t VALUES (3, 'unique-2@x')")
        .unwrap();
    eng.execute("INSERT INTO t VALUES (4, 'dup@x')").unwrap();
    eng.execute("INSERT INTO t VALUES (5, 'unique-3@x')")
        .unwrap();
    // Freeze id=1 (dup row) to cold.
    eng.freeze_oldest_to_cold("t", "t_pkey", 1)
        .expect("freeze dup");
    // CREATE UNIQUE INDEX must catch the (cold dup, hot dup) collision.
    let res = eng.execute("CREATE UNIQUE INDEX uq_email ON t (email)");
    assert!(
        res.is_err(),
        "CREATE UNIQUE INDEX must detect cold-tier duplicate"
    );
}

/// v7.36 — MERGE INTO target must error when the target has
/// cold-tier rows. MATCHED clauses would silently skip cold target
/// rows otherwise — losing the upsert semantic. Source-side cold
/// rows are read-only inputs and are folded in automatically.
#[test]
fn merge_into_target_blocks_when_target_has_cold_rows() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE target_t (id BIGINT NOT NULL PRIMARY KEY, val BIGINT NOT NULL)")
        .unwrap();
    eng.execute("CREATE TABLE source_t (id BIGINT NOT NULL PRIMARY KEY, val BIGINT NOT NULL)")
        .unwrap();
    for i in 1..=10 {
        eng.execute(&format!("INSERT INTO target_t VALUES ({i}, {i})"))
            .unwrap();
    }
    eng.execute("INSERT INTO source_t VALUES (3, 333)").unwrap();
    eng.freeze_oldest_to_cold("target_t", "target_t_pkey", 5)
        .expect("freeze target");
    let err = eng
        .execute(
            "MERGE INTO target_t t USING source_t s ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET val = s.val",
        )
        .expect_err("MERGE on cold-bearing target must error");
    let msg = format!("{err}");
    assert!(
        msg.contains("cold-tier"),
        "expected cold-tier MERGE error, got: {msg}"
    );
}

/// v7.36 — UPDATE with a non-PK WHERE on a cold-bearing table.
/// Pre-7.36 only PK-eq WHERE promoted the matching cold row to
/// hot before the SET loop; predicate WHEREs silently dropped
/// cold matches. Now any matching cold rows are pre-promoted so
/// the regular hot walk picks them up.
#[test]
fn update_non_pk_where_promotes_cold_matches() {
    let mut eng = Engine::new();
    eng.execute(
        "CREATE TABLE t (id BIGINT NOT NULL PRIMARY KEY, status TEXT NOT NULL, val BIGINT NOT NULL)",
    )
    .unwrap();
    for i in 1..=10 {
        eng.execute(&format!("INSERT INTO t VALUES ({i}, 'pending', {i})"))
            .unwrap();
    }
    eng.freeze_oldest_to_cold("t", "t_pkey", 5).expect("freeze");
    // Non-PK WHERE updating status of every row.
    eng.execute("UPDATE t SET status = 'done' WHERE val > 0")
        .expect("non-PK UPDATE must succeed across both tiers");
    // COUNT(status='done') should be all 10.
    let rows = rows_of(
        eng.execute("SELECT COUNT(*) FROM t WHERE status = 'done'")
            .unwrap(),
    );
    assert_eq!(
        rows[0][0],
        Value::BigInt(10),
        "non-PK UPDATE must reach cold-tier rows too"
    );
}

/// v7.36 — DELETE with a non-PK WHERE on a cold-bearing table.
/// Same shape as UPDATE: pre-7.36 only PK-eq WHERE shadowed the
/// matching cold-tier locator. Now predicate WHEREs walk cold
/// rows, eval, and shadow each match.
#[test]
fn delete_non_pk_where_shadows_cold_matches() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id BIGINT NOT NULL PRIMARY KEY, val BIGINT NOT NULL)")
        .unwrap();
    for i in 1..=10 {
        eng.execute(&format!("INSERT INTO t VALUES ({i}, {i})"))
            .unwrap();
    }
    eng.freeze_oldest_to_cold("t", "t_pkey", 5).expect("freeze");
    // Non-PK WHERE deleting rows with val ≤ 3 — hits ids 1, 2, 3
    // which are all in the cold tier.
    eng.execute("DELETE FROM t WHERE val <= 3")
        .expect("non-PK DELETE must reach cold rows");
    let rows = rows_of(eng.execute("SELECT COUNT(*) FROM t").unwrap());
    assert_eq!(
        rows[0][0],
        Value::BigInt(7),
        "non-PK DELETE must shadow cold-tier matches"
    );
}

#[test]
fn cold_row_count_zero_before_any_freeze() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    eng.execute("CREATE INDEX ix_t ON t (id)").unwrap();
    for i in 0..5 {
        eng.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
    }
    eng.execute("ANALYZE t").unwrap();
    let stats = rows_of(eng.execute("SELECT * FROM spg_statistic").unwrap());
    for row in &stats {
        assert_eq!(row[5], Value::BigInt(0), "no freeze → cold_row_count = 0");
    }
}
