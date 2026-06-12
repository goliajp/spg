//! mailrs round-26 — the meili backfill query VERBATIM on the
//! prepared (sqlx inline) path, typed (TESTING.md acceptance-shape
//! rules 2+3). This is the exact statement that drove the prod
//! reclaim-livelock incident: placeholders for the keyset cursor and
//! the batch size, a JOIN to mailboxes, ORDER BY id, LIMIT. v7.30.3
//! executes it through the bounded top-N path; these tests pin the
//! row-level semantics so the knife can never drift from the general
//! path on the one query mailrs runs hourly.

use spg_sqlx::{SpgPool, SpgPoolExt};

const BACKFILL_SQL: &str = "SELECT m.id, m.thread_id, m.subject, m.sender, m.recipients, m.text_body, m.clean_text, m.internal_date, mb.user_address \
     FROM messages m JOIN mailboxes mb ON m.mailbox_id = mb.id \
     WHERE m.id > $1 \
     ORDER BY m.id ASC LIMIT $2";

/// mailrs's own row tuple (crates/server/src/search_index.rs).
type MessageRow = (
    i64,
    String,
    Option<String>,
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    String,
);

async fn seeded_pool() -> SpgPool {
    let pool = SpgPool::connect_in_memory().await.expect("in-memory spg");
    for ddl in [
        "CREATE TABLE messages (id BIGINT, thread_id TEXT, subject TEXT, sender TEXT, \
         recipients TEXT, text_body TEXT, clean_text TEXT, internal_date BIGINT, mailbox_id BIGINT)",
        "CREATE TABLE mailboxes (id BIGINT, user_address TEXT)",
        "INSERT INTO mailboxes VALUES (1, 'inbox@example.com'), (2, 'archive@example.com')",
        // id 2 carries NULL subject / text_body / clean_text (the
        // Option slots); id 3 is an orphan (mailbox 9 — INNER drops);
        // id 4 lands in mailbox 2.
        "INSERT INTO messages VALUES \
         (1, 't1', 'hello', 'a@x', 'b@x', 'body one', 'clean one', 100, 1), \
         (2, 't2', NULL, 'c@x', 'd@x', NULL, NULL, 200, 1), \
         (3, 't3', 's3', 'e@x', 'f@x', 'orphan', 'orphan', 300, 9), \
         (4, 't4', 's4', 'g@x', 'h@x', 'body four', 'clean four', 400, 2)",
    ] {
        sqlx::query(ddl).execute(&pool).await.unwrap();
    }
    pool
}

#[tokio::test]
async fn backfill_first_batch_decodes_typed_and_ordered() {
    let pool = seeded_pool().await;
    let rows: Vec<MessageRow> = sqlx::query_as(BACKFILL_SQL)
        .bind(0_i64)
        .bind(2_i64)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    let (id, thread, subject, sender, _, body, clean, ts, addr) = rows[0].clone();
    assert_eq!(
        (id, thread.as_str(), sender.as_str(), ts, addr.as_str()),
        (1, "t1", "a@x", 100, "inbox@example.com")
    );
    assert_eq!(subject.as_deref(), Some("hello"));
    assert_eq!(body.as_deref(), Some("body one"));
    assert_eq!(clean.as_deref(), Some("clean one"));
    // Row 2: every Option slot NULL.
    let (id2, _, subject2, _, _, body2, clean2, _, _) = rows[1].clone();
    assert_eq!(id2, 2);
    assert_eq!(subject2, None);
    assert_eq!(body2, None);
    assert_eq!(clean2, None);
}

#[tokio::test]
async fn backfill_keyset_pagination_resumes_past_cursor() {
    let pool = seeded_pool().await;
    // Cursor after the first batch: m.id > 2 — the orphan (id 3,
    // mailbox 9) must NOT appear; the next joined row is id 4.
    let rows: Vec<MessageRow> = sqlx::query_as(BACKFILL_SQL)
        .bind(2_i64)
        .bind(1000_i64)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, 4);
    assert_eq!(rows[0].8, "archive@example.com");
}

#[tokio::test]
async fn backfill_caught_up_returns_empty() {
    let pool = seeded_pool().await;
    let rows: Vec<MessageRow> = sqlx::query_as(BACKFILL_SQL)
        .bind(4_i64)
        .bind(1000_i64)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(rows.is_empty());
}
