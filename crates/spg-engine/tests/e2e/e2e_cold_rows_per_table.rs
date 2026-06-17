//! v6.7.0 — per-table cold_rows precise count.
//!
//! Resolves the v6.2.7 STABILITY carve-out: spg_statistic gains a
//! cold_row_count column populated by ANALYZE; spg_stat_segment
//! gains a table_name column populated by walking BTree-index Cold
//! locators.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows_of(res: QueryResult) -> Vec<Vec<Value>> {
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
            Value::Text("t".to_string()),
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
    assert_eq!(by_id[0][0], Value::Text("mb-5".to_string()));
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
