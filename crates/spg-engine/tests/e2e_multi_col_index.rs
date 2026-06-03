//! v7.9.14 — F2: multi-column CREATE INDEX parses cleanly.
//! Storage today builds a single-column BTree on the leading
//! column; extras live on the AST until v7.10 lands composite
//! BTree keys. The parser-side accept is the unblock for
//! mailrs' init-schema.sql.

use spg_engine::{Engine, QueryResult};

fn engine_with(sqls: &[&str]) -> Engine {
    let mut eng = Engine::new();
    for sql in sqls {
        let r = eng
            .execute(sql)
            .unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
        assert!(matches!(r, QueryResult::CommandOk { .. }), "{sql:?}");
    }
    eng
}

#[test]
fn multi_column_create_index_parses_and_builds_leading_index() {
    // mailrs hit: `CREATE INDEX idx_messages_date ON messages(mailbox_id, date_epoch DESC)`
    let eng = engine_with(&[
        "CREATE TABLE messages (id INT NOT NULL, mailbox_id INT NOT NULL, date_epoch BIGINT NOT NULL)",
        "CREATE INDEX idx_messages_date ON messages (mailbox_id, date_epoch DESC)",
    ]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let idx = cat
        .get("messages")
        .unwrap()
        .indices()
        .iter()
        .find(|i| i.name == "idx_messages_date")
        .expect("index built");
    // The leading column drives the storage; extras are AST-only.
    assert!(matches!(idx.kind, spg_storage::IndexKind::BTree(_)));
}

#[test]
fn multi_column_index_with_partial_predicate_parses() {
    // mailrs hit: `CREATE INDEX idx_queue_pending ON outbound_queue(status, next_retry) WHERE status = 'pending'`
    let _eng = engine_with(&[
        "CREATE TABLE outbound_queue (id INT NOT NULL, status TEXT NOT NULL, next_retry BIGINT)",
        "CREATE INDEX idx_queue_pending ON outbound_queue (status, next_retry) \
         WHERE status = 'pending'",
    ]);
}

#[test]
fn multi_column_index_with_asc_desc_on_each_col() {
    let _eng = engine_with(&[
        "CREATE TABLE t (a INT NOT NULL, b INT NOT NULL, c INT NOT NULL)",
        "CREATE INDEX t_abc ON t (a ASC, b DESC, c)",
    ]);
}

#[test]
fn multi_column_index_with_nulls_last() {
    let _eng = engine_with(&[
        "CREATE TABLE t (a INT NOT NULL, b INT)",
        "CREATE INDEX t_b_nulls ON t (a, b DESC NULLS LAST)",
    ]);
}

#[test]
fn single_column_index_still_works_after_widening() {
    let eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_id ON u (id)",
    ]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    assert!(cat.get("u").unwrap().indices().iter().any(|i| i.name == "u_id"));
}

#[test]
fn three_col_index_with_mixed_ordering_parses() {
    // Contact ranking pattern from mailrs:
    // CREATE INDEX idx_contacts_user_score
    //   ON contacts (user_address, relationship_score DESC, last_seen DESC)
    let _eng = engine_with(&[
        "CREATE TABLE contacts (user_address TEXT NOT NULL, relationship_score INT NOT NULL, last_seen BIGINT NOT NULL)",
        "CREATE INDEX idx_contacts_user_score ON contacts (user_address, relationship_score DESC, last_seen DESC)",
    ]);
}
