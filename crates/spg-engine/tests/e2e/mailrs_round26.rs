//! v7.30.3 (mailrs round-26) — bounded join execution + per-query
//! byte budget. The prod incident: the meili backfill shape
//! `SELECT … FROM messages m JOIN mailboxes mb ON m.mailbox_id =
//! mb.id WHERE m.id > $1 ORDER BY m.id ASC LIMIT $2` materialised
//! the FULL join (≈2× every mail body) before LIMIT truncated,
//! walking a 15 GiB no-swap host into reclaim livelock. Pins:
//! streamed top-N semantics across LIMIT / OFFSET / DESC / ties /
//! no-ORDER shapes, fallback parity for shapes the knife declines
//! (LEFT JOIN, aggregates), and the byte budget's honest error.

use spg_engine::{Engine, EngineError, QueryResult};
use spg_storage::Value;

fn ok(eng: &mut Engine, sql: &str) -> QueryResult {
    eng.execute(sql)
        .unwrap_or_else(|e| panic!("{sql:?}: {e:?}"))
}

fn rows(eng: &mut Engine, sql: &str) -> Vec<Vec<Value>> {
    match ok(eng, sql) {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("expected Rows for {sql:?}, got {other:?}"),
    }
}

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::BigInt(n) => *n,
        Value::Int(n) => i64::from(*n),
        Value::SmallInt(n) => i64::from(*n),
        other => panic!("expected integer, got {other:?}"),
    }
}

/// Seeded fixture (TESTING.md acceptance-shape rule 1): rows present,
/// a NULL in the fat column, a NULL in a projected column, and an
/// orphan message whose mailbox does not exist (INNER drops it).
fn seeded() -> Engine {
    let mut e = Engine::new();
    ok(
        &mut e,
        "CREATE TABLE messages (id BIGINT, mailbox_id BIGINT, subject TEXT, text_body TEXT)",
    );
    ok(
        &mut e,
        "CREATE TABLE mailboxes (id BIGINT, user_address TEXT)",
    );
    ok(
        &mut e,
        "INSERT INTO mailboxes VALUES (1, 'a@example.com'), (2, 'b@example.com')",
    );
    ok(
        &mut e,
        "INSERT INTO messages VALUES \
         (1, 1, 's1', 'body-1'), \
         (2, 2, 's2', NULL), \
         (3, 1, 's3', 'body-3'), \
         (4, 9, 's4', 'orphan-no-mailbox'), \
         (5, 2, 's5', 'body-5'), \
         (6, 1, NULL, 'body-6')",
    );
    e
}

const BACKFILL: &str = "SELECT m.id, m.subject, m.text_body, mb.user_address \
     FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id \
     WHERE m.id > 1 ORDER BY m.id ASC LIMIT 3";

#[test]
fn backfill_shape_limit_orders_and_projects() {
    let mut e = seeded();
    let got = rows(&mut e, BACKFILL);
    // id > 1, joined (4 is an orphan): 2, 3, 5.
    assert_eq!(got.len(), 3, "{got:?}");
    assert_eq!(as_i64(&got[0][0]), 2);
    assert!(
        matches!(got[0][2], Value::Null),
        "NULL body survives: {got:?}"
    );
    assert_eq!(got[0][3], Value::Text("b@example.com".into()));
    assert_eq!(as_i64(&got[1][0]), 3);
    assert_eq!(got[1][3], Value::Text("a@example.com".into()));
    assert_eq!(as_i64(&got[2][0]), 5);
    assert_eq!(got[2][2], Value::Text("body-5".into()));
}

#[test]
fn limit_larger_than_matches_returns_all() {
    let mut e = seeded();
    let got = rows(
        &mut e,
        "SELECT m.id FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id \
         ORDER BY m.id ASC LIMIT 100",
    );
    let ids: Vec<i64> = got.iter().map(|r| as_i64(&r[0])).collect();
    assert_eq!(ids, vec![1, 2, 3, 5, 6]);
}

#[test]
fn offset_skips_in_order() {
    let mut e = seeded();
    let got = rows(
        &mut e,
        "SELECT m.id FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id \
         WHERE m.id > 1 ORDER BY m.id ASC LIMIT 2 OFFSET 1",
    );
    let ids: Vec<i64> = got.iter().map(|r| as_i64(&r[0])).collect();
    assert_eq!(ids, vec![3, 5]);
}

#[test]
fn desc_order_takes_the_top_end() {
    let mut e = seeded();
    let got = rows(
        &mut e,
        "SELECT m.id FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id \
         ORDER BY m.id DESC LIMIT 2",
    );
    let ids: Vec<i64> = got.iter().map(|r| as_i64(&r[0])).collect();
    assert_eq!(ids, vec![6, 5]);
}

#[test]
fn tied_order_keys_keep_exactly_limit_rows() {
    let mut e = seeded();
    // mailbox_id 1 ties three ways (ids 1, 3, 6).
    let got = rows(
        &mut e,
        "SELECT m.mailbox_id, m.id FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id \
         ORDER BY m.mailbox_id ASC LIMIT 2",
    );
    assert_eq!(got.len(), 2);
    for r in &got {
        assert_eq!(
            as_i64(&r[0]),
            1,
            "ties must come from the smallest key: {got:?}"
        );
        assert!([1, 3, 6].contains(&as_i64(&r[1])), "{got:?}");
    }
}

#[test]
fn no_order_by_stops_at_limit() {
    let mut e = seeded();
    let got = rows(
        &mut e,
        "SELECT m.id FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id \
         WHERE m.id > 1 LIMIT 2",
    );
    assert_eq!(got.len(), 2);
    for r in &got {
        let id = as_i64(&r[0]);
        assert!(
            [2, 3, 5].contains(&id),
            "row outside WHERE+join set: {got:?}"
        );
    }
}

#[test]
fn limit_zero_yields_schema_and_no_rows() {
    let mut e = seeded();
    match ok(
        &mut e,
        "SELECT m.id, mb.user_address FROM messages m JOIN mailboxes mb \
         ON m.mailbox_id = mb.id ORDER BY m.id LIMIT 0",
    ) {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns.len(), 2);
            assert!(rows.is_empty());
        }
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn left_join_falls_back_and_null_extends() {
    let mut e = seeded();
    let got = rows(
        &mut e,
        "SELECT m.id, mb.user_address FROM messages m LEFT JOIN mailboxes mb \
         ON m.mailbox_id = mb.id WHERE m.id > 3 ORDER BY m.id ASC LIMIT 5",
    );
    let ids: Vec<i64> = got.iter().map(|r| as_i64(&r[0])).collect();
    assert_eq!(ids, vec![4, 5, 6]);
    assert!(
        matches!(got[0][1], Value::Null),
        "orphan NULL-extends: {got:?}"
    );
    assert_eq!(got[1][1], Value::Text("b@example.com".into()));
}

#[test]
fn streamed_path_matches_unlimited_general_path() {
    let mut e = seeded();
    let unlimited = rows(
        &mut e,
        "SELECT m.id, m.text_body, mb.user_address FROM messages m \
         JOIN mailboxes mb ON m.mailbox_id = mb.id ORDER BY m.id ASC",
    );
    let knifed = rows(
        &mut e,
        "SELECT m.id, m.text_body, mb.user_address FROM messages m \
         JOIN mailboxes mb ON m.mailbox_id = mb.id ORDER BY m.id ASC LIMIT 1000",
    );
    assert_eq!(unlimited, knifed);
}

#[test]
fn where_subquery_routes_through_correlated_eval() {
    let mut e = seeded();
    let got = rows(
        &mut e,
        "SELECT m.id FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id \
         WHERE m.id > (SELECT MIN(id) FROM messages) ORDER BY m.id ASC LIMIT 2",
    );
    let ids: Vec<i64> = got.iter().map(|r| as_i64(&r[0])).collect();
    assert_eq!(ids, vec![2, 3]);
}

#[test]
fn aggregates_bypass_the_knife() {
    let mut e = seeded();
    let got = rows(
        &mut e,
        "SELECT COUNT(*) FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id LIMIT 1",
    );
    assert_eq!(as_i64(&got[0][0]), 5);
}

#[test]
fn byte_budget_trips_general_path_with_honest_error() {
    // LEFT JOIN declines the knife, so the general materialisation
    // path runs under a deliberately tiny cap.
    let mut e = seeded_with_budget(64);
    let err = e
        .execute(
            "SELECT m.id, m.text_body FROM messages m LEFT JOIN mailboxes mb \
             ON m.mailbox_id = mb.id ORDER BY m.id LIMIT 2",
        )
        .unwrap_err();
    assert!(
        matches!(err, EngineError::QueryBytesExceeded(64)),
        "{err:?}"
    );
    let msg = format!("{err}");
    assert!(msg.contains("max_query_bytes=64"), "{msg}");
    assert!(msg.contains("SPG_MAX_QUERY_BYTES"), "{msg}");
}

#[test]
fn byte_budget_trips_streamed_path_on_fat_peer() {
    let mut e = seeded_with_budget(64);
    let err = e
        .execute(
            "SELECT m.id FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id \
             ORDER BY m.id LIMIT 1",
        )
        .unwrap_err();
    assert!(
        matches!(err, EngineError::QueryBytesExceeded(64)),
        "{err:?}"
    );
}

#[test]
fn byte_budget_spares_sub_budget_queries() {
    let mut e = seeded_with_budget(1024 * 1024);
    let got = rows(&mut e, BACKFILL);
    assert_eq!(got.len(), 3);
}

#[test]
fn writes_ignore_the_budget() {
    // INSERT / UPDATE never route through the join materialiser; a
    // tiny budget must not break ingest.
    let mut e = seeded_with_budget(16);
    ok(&mut e, "INSERT INTO messages VALUES (7, 1, 's7', 'body-7')");
    ok(&mut e, "UPDATE messages SET subject = 's7b' WHERE id = 7");
}

fn seeded_with_budget(bytes: usize) -> Engine {
    let mut e = Engine::new().with_max_query_bytes(bytes);
    ok(
        &mut e,
        "CREATE TABLE messages (id BIGINT, mailbox_id BIGINT, subject TEXT, text_body TEXT)",
    );
    ok(
        &mut e,
        "CREATE TABLE mailboxes (id BIGINT, user_address TEXT)",
    );
    ok(
        &mut e,
        "INSERT INTO mailboxes VALUES (1, 'a@example.com'), (2, 'b@example.com')",
    );
    ok(
        &mut e,
        "INSERT INTO messages VALUES \
         (1, 1, 's1', 'body-1'), \
         (2, 2, 's2', NULL), \
         (3, 1, 's3', 'body-3'), \
         (4, 9, 's4', 'orphan-no-mailbox'), \
         (5, 2, 's5', 'body-5'), \
         (6, 1, NULL, 'body-6')",
    );
    e
}
