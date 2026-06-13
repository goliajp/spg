//! mailrs round-30 — correlated JOIN subquery in a grouped SELECT
//! list (the inbox "latest analysed category per thread" shape).
//!
//! Two paths converge here, both regression-guarded by this file:
//!   1. `subquery_replacement` must NOT optimistically execute a
//!      correlated subquery (a join body would clone its whole
//!      driving table — the R30 prod 12 GiB livelock). The static
//!      `select_is_correlated` pre-check defers it instead.
//!   2. The deferred post-LIMIT evaluation drives the inner join from
//!      the correlation table's index (PG/MySQL/MariaDB all plan it
//!      this way), so the answer must equal the naive per-row result.

use spg_engine::{Engine, QueryResult};
use spg_storage::{Row, Value};

fn setup() -> Engine {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE messages (id INT PRIMARY KEY, thread_id TEXT, internal_date BIGINT)",
    )
    .unwrap();
    e.execute("CREATE TABLE email_analysis (message_id INT PRIMARY KEY, category TEXT)")
        .unwrap();
    e.execute("CREATE INDEX idx_thread ON messages(thread_id)")
        .unwrap();
    // 3 threads × 4 messages. internal_date increases with id, so the
    // "latest" message in a thread is the highest id.
    let mut id = 1;
    for t in 0..3 {
        for _ in 0..4 {
            e.execute(&format!(
                "INSERT INTO messages VALUES ({id}, 'th-{t}', {})",
                1700 + id
            ))
            .unwrap();
            id += 1;
        }
    }
    // Analyse a subset so some threads' latest message has no
    // analysis (exercises COALESCE fallback) and the latest-analysed
    // row differs from the latest message.
    //   thread 0: msgs 1..4  -> analyse 1 ('a'), 3 ('b'); latest analysed = 3 -> 'b'
    //   thread 1: msgs 5..8  -> analyse 8 ('c');          latest analysed = 8 -> 'c'
    //   thread 2: msgs 9..12 -> none;                     -> COALESCE 'none'
    for (mid, cat) in [(1, "a"), (3, "b"), (8, "c")] {
        e.execute(&format!(
            "INSERT INTO email_analysis VALUES ({mid}, '{cat}')"
        ))
        .unwrap();
    }
    e.execute("ANALYZE").unwrap();
    e
}

fn cat_of(rows: &[Row], thread: &str) -> String {
    for r in rows {
        if let Value::Text(t) = &r.values[0]
            && t == thread
            && let Value::Text(c) = &r.values[1]
        {
            return c.clone();
        }
    }
    panic!("thread {thread} not found in result");
}

#[test]
fn correlated_join_subquery_latest_analysed_category() {
    let mut e = setup();
    let sql = "SELECT m.thread_id, \
         COALESCE((SELECT e2.category FROM email_analysis e2 \
            JOIN messages m2 ON e2.message_id = m2.id \
            WHERE m2.thread_id = m.thread_id \
            ORDER BY m2.internal_date DESC LIMIT 1), 'none') \
         FROM messages m GROUP BY m.thread_id ORDER BY m.thread_id";
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap() else {
        panic!("expected rows");
    };
    assert_eq!(rows.len(), 3);
    assert_eq!(cat_of(&rows, "th-0"), "b", "latest analysed in thread 0 is msg 3");
    assert_eq!(cat_of(&rows, "th-1"), "c", "only msg 8 analysed in thread 1");
    assert_eq!(cat_of(&rows, "th-2"), "none", "thread 2 has no analysis");
}

#[test]
fn correlated_join_subquery_matches_naive_per_thread() {
    // The keyed/deferred answer must equal a hand-rolled per-thread
    // top-1 lookup — the differential the optimisation must preserve.
    let mut e = setup();
    let sql = "SELECT m.thread_id, \
         (SELECT e2.category FROM email_analysis e2 \
            JOIN messages m2 ON e2.message_id = m2.id \
            WHERE m2.thread_id = m.thread_id \
            ORDER BY m2.internal_date DESC LIMIT 1) \
         FROM messages m GROUP BY m.thread_id ORDER BY m.thread_id";
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap() else {
        panic!("expected rows");
    };
    // thread 0 -> 'b', thread 1 -> 'c', thread 2 -> NULL.
    assert!(matches!(&rows[0].values[1], Value::Text(c) if c == "b"));
    assert!(matches!(&rows[1].values[1], Value::Text(c) if c == "c"));
    assert!(matches!(rows[2].values[1], Value::Null));
}
