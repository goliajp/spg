//! v5.1: two-tier (hot + cold) read path end-to-end through
//! `Engine::execute`. Exercises the four corner cases the v5.1
//! ship gate calls out:
//!
//!   1. PK lookup on a table whose rows all live in the hot tier
//!      (the pre-v5 baseline — no cold segments registered).
//!   2. PK lookup on a table whose rows all live in a cold-tier
//!      segment (hot tier is empty for those keys; the engine
//!      must dispatch through `Catalog::resolve_cold_locator`).
//!   3. PK lookup on a table that has *both* hot rows and a cold
//!      segment, with random keys distributed across both tiers.
//!   4. PK lookup for a key the cold segment definitely doesn't
//!      carry — the bloom + page-index + page-scan stack must all
//!      reject without surfacing an error.
//!
//! Cold segments are hand-baked here (instead of going through
//! the v5.2 freezer, which is what would produce them in
//! production). Each row body is dense-encoded with
//! `encode_row_body_dense` so the catalog's reverse decode in
//! `resolve_cold_locator` round-trips to the right `Row`.

use spg_engine::{Engine, QueryResult};
use spg_storage::{
    Catalog, IndexKey, RowLocator, SEGMENT_PAGE_BYTES, Value, encode_row_body_dense, encode_segment,
};

/// CREATE the table the suite uses (BIGINT PK + TEXT label) and
/// build a BTree index on its PK. Cold-tier paths require an
/// indexed BIGINT/INT PK in v5.1.
fn boot_engine_with_users() -> Engine {
    let mut engine = Engine::new();
    engine
        .execute("CREATE TABLE users (id BIGINT NOT NULL, name TEXT NOT NULL)")
        .expect("create table");
    engine
        .execute("CREATE INDEX users_by_id ON users (id)")
        .expect("create index");
    engine
}

/// Append a cold-tier segment containing the given (id, name)
/// rows to the engine's catalog, then register one
/// `RowLocator::Cold` per key on `users_by_id`. The hot tier is
/// untouched.
fn register_cold_users(engine: &mut Engine, rows: &[(i64, &str)]) -> u32 {
    let schema = engine
        .catalog()
        .get("users")
        .expect("table exists")
        .schema()
        .clone();
    let seg_rows: Vec<(u64, Vec<u8>)> = rows
        .iter()
        .map(|(id, name)| {
            let row = spg_storage::Row::new(vec![Value::BigInt(*id), Value::Text((*name).into())]);
            (*id as u64, encode_row_body_dense(&row, &schema))
        })
        .collect();
    let (seg_bytes, _) =
        encode_segment(seg_rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).expect("encode segment");

    let mut cat: Catalog = engine.catalog().clone();
    let seg_id = cat.load_segment_bytes(seg_bytes).expect("load segment");
    let pairs: Vec<(IndexKey, RowLocator)> = rows
        .iter()
        .map(|(id, _)| {
            (
                IndexKey::Int(*id),
                RowLocator::Cold {
                    segment_id: seg_id,
                    page_offset: 0,
                },
            )
        })
        .collect();
    cat.get_mut("users")
        .expect("table exists")
        .register_cold_locators("users_by_id", pairs)
        .expect("register cold locators");
    engine.replace_catalog(cat);
    seg_id
}

/// Run `SELECT name FROM users WHERE id = <id>` and return the
/// resulting (single) `name` value, or `None` on zero rows.
fn select_name_by_id(engine: &mut Engine, id: i64) -> Option<String> {
    let q = format!("SELECT name FROM users WHERE id = {id}");
    let r = engine.execute(&q).expect("select runs");
    let (_cols, rows) = match r {
        QueryResult::Rows { columns, rows } => (columns, rows),
        QueryResult::CommandOk { .. } => panic!("expected Rows"),
    };
    if rows.is_empty() {
        return None;
    }
    assert_eq!(rows.len(), 1, "PK lookup must return at most one row");
    let v = &rows[0].values[0];
    match v {
        Value::Text(s) => Some(s.clone()),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn pk_lookup_finds_row_in_hot_only_table() {
    // Baseline: no cold segments, just hot inserts. Verifies the
    // v5.1 `try_index_seek` Cow path doesn't regress hot lookup.
    let mut engine = boot_engine_with_users();
    for (id, name) in [(1i64, "alice"), (2, "bob"), (3, "carol")] {
        engine
            .execute(&format!("INSERT INTO users VALUES ({id}, '{name}')"))
            .expect("insert");
    }
    assert_eq!(engine.catalog().cold_segment_count(), 0);
    assert_eq!(select_name_by_id(&mut engine, 1).as_deref(), Some("alice"));
    assert_eq!(select_name_by_id(&mut engine, 2).as_deref(), Some("bob"));
    assert_eq!(select_name_by_id(&mut engine, 3).as_deref(), Some("carol"));
    assert!(select_name_by_id(&mut engine, 999).is_none());
}

#[test]
fn pk_lookup_finds_row_in_cold_only_table() {
    // Every row lives in a hand-baked segment; the hot tier
    // (`Table::rows`) is empty. Each PK lookup must dispatch
    // through `Catalog::resolve_cold_locator`.
    let mut engine = boot_engine_with_users();
    let cold: Vec<(i64, &str)> = vec![
        (100, "ivy"),
        (200, "joe"),
        (300, "kim"),
        (400, "lin"),
        (500, "mae"),
    ];
    register_cold_users(&mut engine, &cold);
    assert_eq!(engine.catalog().cold_segment_count(), 1);
    for (id, name) in &cold {
        assert_eq!(
            select_name_by_id(&mut engine, *id).as_deref(),
            Some(*name),
            "cold-tier lookup for id={id} should return {name}"
        );
    }
    // Hot tier was never populated for these keys.
    assert_eq!(
        engine.catalog().get("users").unwrap().row_count(),
        0,
        "hot tier should be empty"
    );
}

#[test]
fn pk_lookup_finds_row_in_either_tier() {
    // Half the keys go through INSERT (hot tier, `RowLocator::Hot`);
    // half are wired via `register_cold_users` (cold tier,
    // `RowLocator::Cold`). The engine must pick the right tier
    // per key. This is the v5.1 ship-gate test the V5_DESIGN.md
    // L4 trigger names explicitly.
    let mut engine = boot_engine_with_users();
    let hot: Vec<(i64, &str)> = vec![(1, "alice"), (2, "bob"), (3, "carol")];
    for (id, name) in &hot {
        engine
            .execute(&format!("INSERT INTO users VALUES ({id}, '{name}')"))
            .expect("insert");
    }
    let cold: Vec<(i64, &str)> = vec![(100, "ivy"), (200, "joe"), (300, "kim")];
    register_cold_users(&mut engine, &cold);

    // Hot hits.
    for (id, name) in &hot {
        assert_eq!(
            select_name_by_id(&mut engine, *id).as_deref(),
            Some(*name),
            "hot id={id} should return {name}"
        );
    }
    // Cold hits.
    for (id, name) in &cold {
        assert_eq!(
            select_name_by_id(&mut engine, *id).as_deref(),
            Some(*name),
            "cold id={id} should return {name}"
        );
    }
    // Gap key (between hot and cold ranges) must miss.
    assert!(select_name_by_id(&mut engine, 50).is_none());
    // Above-everything key must miss.
    assert!(select_name_by_id(&mut engine, 10_000).is_none());
}

#[test]
fn cold_pk_lookup_returns_none_for_missing_key() {
    // Bloom + page-index + page-scan must all reject keys outside
    // the cold segment's content. The lookup returns zero rows,
    // not an error.
    let mut engine = boot_engine_with_users();
    let cold: Vec<(i64, &str)> = vec![(100, "ivy"), (200, "joe"), (300, "kim")];
    register_cold_users(&mut engine, &cold);

    // Below min_pk → segment header out-of-range rejects.
    assert!(select_name_by_id(&mut engine, 1).is_none());
    // Above max_pk → segment header out-of-range rejects.
    assert!(select_name_by_id(&mut engine, 1_000).is_none());
    // Between cold-tier keys (the index has no entry → seek
    // returns an empty locator slice, lookup_by_pk returns None
    // before ever touching the segment).
    assert!(select_name_by_id(&mut engine, 150).is_none());
    // Key inside [min_pk, max_pk] but absent — would reach the
    // segment's page-internal binary search if registered. We
    // exercise that path by registering a stale Cold locator for
    // a key the segment doesn't actually carry, so the engine
    // walks bloom + page-index + page-scan and finds nothing.
    let mut cat = engine.catalog().clone();
    cat.get_mut("users")
        .unwrap()
        .register_cold_locators(
            "users_by_id",
            vec![(
                IndexKey::Int(250),
                RowLocator::Cold {
                    segment_id: 0,
                    page_offset: 0,
                },
            )],
        )
        .unwrap();
    engine.replace_catalog(cat);
    assert!(
        select_name_by_id(&mut engine, 250).is_none(),
        "stale Cold locator for key not in segment should yield no row"
    );
}
