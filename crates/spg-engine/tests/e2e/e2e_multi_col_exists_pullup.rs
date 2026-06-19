//! v7.37.4 A'' — multi-column NOT EXISTS pullup differential e2e.
//! The new pullup path turns a 2-key NOT EXISTS into a LEFT JOIN +
//! IS NULL anti-join. Pin byte-equal result against the legacy
//! batch-resolver path via the EXISTS_PULLUP_MULTICOL_DISABLE knob.

use spg_engine::{EXISTS_PULLUP_FIRE_COUNT, EXISTS_PULLUP_MULTICOL_DISABLE, Engine, QueryResult};
use spg_storage::Value;
use std::sync::atomic::Ordering;

fn rows_of(e: &mut Engine, sql: &str) -> Vec<spg_storage::Row> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows from {sql:?}, got {other:?}"),
    }
}

/// Mailrs-shaped fixture: a `messages` outer table joined to
/// `mailboxes`, with a `snoozed_conversations` anti-join correlated
/// on two keys (`thread_id`, `account_address`). The outer also
/// references bare `thread_id` in WHERE — proving the A''
/// disambiguation pass works.
fn setup_mailrs_like() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE mailboxes (id BIGINT PRIMARY KEY, user_address TEXT, name TEXT)")
        .unwrap();
    e.execute(
        "CREATE TABLE messages (id BIGINT PRIMARY KEY, mailbox_id BIGINT, thread_id TEXT, subject TEXT)",
    )
    .unwrap();
    e.execute(
        "CREATE TABLE snoozed_conversations (thread_id TEXT, account_address TEXT, snoozed_until BIGINT)",
    )
    .unwrap();
    e.execute(
        "INSERT INTO mailboxes (id, user_address, name) VALUES \
         (1, 'a@example.com', 'INBOX'), (2, 'a@example.com', 'Sent'), \
         (3, 'b@example.com', 'INBOX')",
    )
    .unwrap();
    e.execute(
        "INSERT INTO messages (id, mailbox_id, thread_id, subject) VALUES \
         (10, 1, 't1', 'hello'), (11, 1, 't1', 'hello-2'), \
         (12, 1, 't2', 'subject-2'), \
         (13, 2, 't3', 'sent-3'), \
         (14, 3, 't4', 'other-user-4'), \
         (15, 1, '', 'empty-thread-skip')",
    )
    .unwrap();
    // Snooze (t1, a@example.com) — should be filtered out by NOT EXISTS.
    // Snooze (t3, b@example.com) — DIFFERENT user — shouldn't affect anything.
    // Snooze (t4, a@example.com) — DIFFERENT user from mailbox — shouldn't
    //   affect t4 since mb 3's user_address = b@.
    e.execute(
        "INSERT INTO snoozed_conversations (thread_id, account_address, snoozed_until) VALUES \
         ('t1', 'a@example.com', 100), \
         ('t3', 'b@example.com', 100), \
         ('t4', 'a@example.com', 100), \
         ('t2', 'a@example.com', 0)",
    )
    .unwrap();
    e
}

const MAILRS_SQL: &str = "\
SELECT m.thread_id, MAX(m.subject) \
  FROM messages m \
       JOIN mailboxes mb ON m.mailbox_id = mb.id \
 WHERE mb.user_address = 'a@example.com' \
   AND thread_id != '' \
   AND NOT EXISTS (SELECT 1 FROM snoozed_conversations sc \
                    WHERE sc.thread_id = m.thread_id \
                      AND sc.account_address = mb.user_address \
                      AND sc.snoozed_until > 0) \
 GROUP BY m.thread_id \
 ORDER BY m.thread_id";

#[test]
fn multi_col_not_exists_pullup_byte_equal_to_batch_baseline() {
    // Run the same SQL twice on isolated engines: pullup-on and
    // pullup-off (multi-col gate disabled). Result rows must match.
    let mut e_on = setup_mailrs_like();
    let mut e_off = setup_mailrs_like();
    let fire_before = EXISTS_PULLUP_FIRE_COUNT.load(Ordering::Relaxed);
    let on = rows_of(&mut e_on, MAILRS_SQL);
    let fired_on = EXISTS_PULLUP_FIRE_COUNT.load(Ordering::Relaxed) - fire_before;
    assert!(
        fired_on >= 1,
        "pullup-on path must trigger ≥1 multi-col EXISTS pullup, got {fired_on}"
    );
    EXISTS_PULLUP_MULTICOL_DISABLE.store(true, Ordering::Relaxed);
    let fire_before = EXISTS_PULLUP_FIRE_COUNT.load(Ordering::Relaxed);
    let off = rows_of(&mut e_off, MAILRS_SQL);
    let fired_off = EXISTS_PULLUP_FIRE_COUNT.load(Ordering::Relaxed) - fire_before;
    EXISTS_PULLUP_MULTICOL_DISABLE.store(false, Ordering::Relaxed);
    assert_eq!(
        fired_off, 0,
        "pullup-off knob must reject multi-col pullup, got {fired_off}"
    );
    assert_eq!(
        on.len(),
        off.len(),
        "row count differs: on={} off={}",
        on.len(),
        off.len()
    );
    for (a, b) in on.iter().zip(off.iter()) {
        assert_eq!(a.values, b.values, "pullup-on vs pullup-off row mismatch");
    }
    // Spot-check semantic content too:
    //   - 't1' is snoozed for a@example.com → drop.
    //   - 't2' has snoozed_until = 0 (predicate sc.snoozed_until > 0 fails)
    //     → survive.
    //   - 't3' is in mb 2 (Sent) — user_address still a@example.com so
    //     it passes the outer mb.user_address = '...' filter; sc 't3' is
    //     for user 'b@example.com', so NOT EXISTS is true → survive.
    //   - 't4' belongs to mb 3 with user_address = b@example.com,
    //     filtered out by `mb.user_address = 'a@example.com'`.
    //   - '' thread_id filtered by `thread_id != ''`.
    let surviving: Vec<&str> = on
        .iter()
        .map(|r| {
            if let Value::Text(s) = &r.values[0] {
                s.as_str()
            } else {
                panic!("expected text thread_id, got {:?}", r.values[0])
            }
        })
        .collect();
    assert_eq!(surviving, vec!["t2", "t3"], "got {surviving:?}");
}

#[test]
fn multi_col_pullup_matches_hand_written_left_join() {
    // Differential vs the hand-written LEFT JOIN + IS NULL anti-join
    // form: the pullup result MUST equal what a developer would have
    // written by hand. The hand form uses an alias to avoid the bare-
    // column ambiguity the disambiguation pass otherwise resolves.
    let mut e = setup_mailrs_like();
    let pulled = rows_of(&mut e, MAILRS_SQL);
    let hand = rows_of(
        &mut e,
        "SELECT m.thread_id, MAX(m.subject) \
           FROM messages m \
                JOIN mailboxes mb ON m.mailbox_id = mb.id \
                LEFT JOIN snoozed_conversations sc \
                       ON sc.thread_id = m.thread_id \
                      AND sc.account_address = mb.user_address \
                      AND sc.snoozed_until > 0 \
          WHERE mb.user_address = 'a@example.com' \
            AND m.thread_id != '' \
            AND sc.thread_id IS NULL \
          GROUP BY m.thread_id \
          ORDER BY m.thread_id",
    );
    assert_eq!(pulled.len(), hand.len(), "row count");
    for (a, b) in pulled.iter().zip(hand.iter()) {
        assert_eq!(a.values, b.values, "pulled vs hand-written");
    }
}

#[test]
fn multi_col_pullup_handles_null_join_keys() {
    // NULL semantics: NOT EXISTS must accept the outer row when ANY
    // correlation column on the inner candidate is NULL (the equality
    // can never hold). LEFT JOIN with NULL on the join key produces a
    // pad row whose key columns are all NULL — IS NULL probe matches —
    // so the outer row survives. Same as per-row resolver.
    let mut e = Engine::new();
    e.execute("CREATE TABLE outr (id INT, k1 TEXT, k2 TEXT)")
        .unwrap();
    e.execute("CREATE TABLE inr (k1 TEXT, k2 TEXT)").unwrap();
    e.execute("INSERT INTO outr VALUES (1, 'a', 'x'), (2, 'a', 'y'), (3, 'b', 'x')")
        .unwrap();
    // inr row matches outer (1, 'a', 'x') exactly. Row 4 has a NULL
    // — its `k2 = outr.k2` is never true, so it never satisfies EXISTS
    // for ANY outer row.
    e.execute("INSERT INTO inr (k1, k2) VALUES ('a', 'x'), ('a', NULL), ('z', 'x')")
        .unwrap();
    let rows = rows_of(
        &mut e,
        "SELECT outr.id FROM outr \
         WHERE NOT EXISTS (SELECT 1 FROM inr \
                            WHERE inr.k1 = outr.k1 \
                              AND inr.k2 = outr.k2) \
         ORDER BY outr.id",
    );
    let ids: Vec<i64> = rows
        .iter()
        .map(|r| match &r.values[0] {
            Value::Int(n) => i64::from(*n),
            Value::BigInt(n) => *n,
            other => panic!("expected int id, got {other:?}"),
        })
        .collect();
    // 1 is in inr → dropped. 2 has no matching inr row → kept. 3 has no
    // matching inr row → kept.
    assert_eq!(ids, vec![2, 3]);
}
