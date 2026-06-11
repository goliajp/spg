//! mailrs round-20 — the TYPED composite gate (mailrs's suggestion):
//! the verbatim search_conversations SQL, seeded, decoded into the
//! exact typed tuple mailrs uses. Pins round-20 C (aggregate column
//! TypeInfo), the round-17/18/19 shapes, and the honest
//! column-not-found error from round-20 B in one test.
//! Note: `.claude/notes/mailrs-embed-round20-column-typeinfo-and-ea.md`.

use spg_sqlx::{SpgPool, SpgPoolExt};

const SEARCH_SQL: &str = include_str!("mailrs_round20_search.sql");

async fn seeded_pool() -> SpgPool {
    let pool = SpgPool::connect_in_memory().await.unwrap();
    for ddl in [
        "CREATE TABLE messages (id BIGSERIAL PRIMARY KEY, uid BIGINT, subject TEXT, sender TEXT, \
         recipients TEXT, text_body TEXT, clean_text TEXT, search_vector tsvector, thread_id TEXT, \
         internal_date BIGINT, flags BIGINT, pinned BOOLEAN, archived BOOLEAN, \
         importance_level TEXT, importance_score REAL, message_id TEXT, mailbox_id BIGINT)",
        "CREATE TABLE mailboxes (id BIGSERIAL PRIMARY KEY, name TEXT, user_address TEXT)",
        "CREATE TABLE email_analysis (message_id BIGINT PRIMARY KEY, category TEXT, summary TEXT, \
         requires_action BOOLEAN NOT NULL DEFAULT false)",
        "CREATE TABLE attachment_content (message_id BIGINT, extracted_text TEXT)",
        "CREATE INDEX idx_thread ON messages(thread_id)",
        "CREATE INDEX idx_sv ON messages USING GIN (search_vector)",
        "INSERT INTO mailboxes (name, user_address) VALUES ('INBOX', 'u@x')",
        "INSERT INTO messages (uid, subject, sender, recipients, text_body, clean_text, \
         search_vector, thread_id, internal_date, flags, pinned, archived, importance_level, \
         importance_score, message_id, mailbox_id) VALUES \
         (1, 'quarterly invoice attached', 'a@b', 'u@x', 'please find the invoice', 'x', \
          to_tsvector('simple','quarterly invoice attached please find the invoice'), \
          'th-1', 100, 0, false, false, 'normal', 0.5, 'mid-1', 1), \
         (2, 're: quarterly invoice attached', 'c@d', 'u@x', 'thanks, received', 'y', \
          to_tsvector('simple','re quarterly invoice attached thanks received'), \
          'th-1', 200, 1, false, false, 'high', 0.9, 'mid-2', 1)",
        "INSERT INTO email_analysis (message_id, category, summary, requires_action) \
         VALUES (1, 'billing', 'pay the invoice', true)",
    ] {
        sqlx::raw_sql(ddl).execute(&pool).await.unwrap();
    }
    pool
}

/// mailrs's exact typed tuple — every column that used to arrive as
/// TEXT (i64 / bool / f32 aggregates) must decode.
#[tokio::test]
async fn search_conversations_typed_tuple_decodes() {
    let pool = seeded_pool().await;
    let rows: Vec<(
        String,         // thread_id
        Option<String>, // MAX(subject)
        Option<String>, // string_agg(DISTINCT sender)
        i64,            // COUNT(DISTINCT …)  (message count)
        i64,            // COUNT(DISTINCT …)  (unread)
        i64,            // MAX(internal_date)
        String,         // COALESCE((SELECT category …), 'general')
        bool,           // BOOL_OR((flags & 4) != 0)
        String,         // COALESCE(snippet…, '')
        bool,           // BOOL_OR(pinned)
        bool,           // BOOL_OR(archived)
        String,         // (array_agg(importance_level …))[1]
        f32,            // COALESCE(MAX(importance_score), 0.0)
        bool,           // COALESCE(BOOL_OR(ea.requires_action), false)
        String,         // (array_agg(sender …))[1]
        i64,            // Sent count
    )> = sqlx::query_as(SEARCH_SQL)
        .bind("u@x")
        .bind("invoice")
        .bind("%invoice%")
        .bind(10_i64)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "one thread group");
    let r = &rows[0];
    assert_eq!(r.0, "th-1");
    assert_eq!(r.3, 2, "two distinct messages");
    assert_eq!(r.4, 1, "one unread (flags&1 == 0)");
    assert_eq!(r.5, 200, "latest internal_date");
    assert_eq!(r.6, "billing", "correlated category column");
    assert!(r.13, "requires_action over LEFT JOIN alias");
    assert_eq!(r.12, 0.9_f32, "max importance score as f32");
    assert_eq!(r.11, "high", "array_agg[1] by importance");
}

/// Round-20 B postscript — a qualified reference to a MISSING column
/// on a joined alias reports "column not found", not the misleading
/// "unknown table qualifier" that sent round-20 hunting a resolver
/// bug (the actual cause was a fixture column missing from
/// init-schema).
#[tokio::test]
async fn missing_joined_column_reports_column_not_found() {
    let pool = seeded_pool().await;
    let err = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM messages m LEFT JOIN email_analysis ea ON ea.message_id = m.id \
         GROUP BY m.thread_id HAVING BOOL_OR(ea.no_such_column)",
    )
    .fetch_all(&pool)
    .await
    .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("ea.no_such_column"), "{msg}");
    assert!(!msg.contains("unknown table qualifier"), "{msg}");
}
