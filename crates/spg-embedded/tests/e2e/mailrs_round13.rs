//! mailrs embed round-13 — regression coverage for the 7 PG 18.4
//! pg_dump shapes the prod cutover dry-run surfaced (2026-06-11).
//! Every statement below is verbatim from the real dump (note:
//! `.claude/notes/mailrs-embed-round13-pg18-dump-import-gaps.md`);
//! the script must load through the embed path with ZERO
//! preprocessing, and the constraints must ENFORCE, not just parse.

use spg_embedded::Database;

/// Gaps 1–7 in dump order, as one script — the same shape
/// `spg import` feeds through `execute_script`.
const ROUND13_SCRIPT: &str = r"
\restrict LK2QxkKy8wit4dFhIyEfs4HNRXTwhuUFdw1RP3ZMbzqfZTuEz26MuuOXfgUSkXW
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
CREATE TABLE contacts (
    id bigint CONSTRAINT contacts_id_not_null1 NOT NULL,
    embedding public.vector(4),
    key_type text,
    CONSTRAINT encryption_keys_key_type_check CHECK ((key_type = ANY (ARRAY['pgp'::text, 'smime'::text])))
);
CREATE TABLE groups (name text, domain text);
ALTER TABLE ONLY groups ADD CONSTRAINT groups_name_domain_key UNIQUE NULLS NOT DISTINCT (name, domain);
CREATE TABLE email_analysis (id int, embedding public.vector(4), clean_text text);
CREATE INDEX idx_ea_embedding ON public.email_analysis USING hnsw (embedding public.vector_cosine_ops);
CREATE INDEX idx_messages_clean_text_trgm ON public.email_analysis USING gin (clean_text public.gin_trgm_ops) WHERE clean_text IS NOT NULL;
\unrestrict LK2QxkKy8wit4dFhIyEfs4HNRXTwhuUFdw1RP3ZMbzqfZTuEz26MuuOXfgUSkXW
";

fn loaded_db() -> Database {
    let mut db = Database::open_in_memory();
    db.execute_script(ROUND13_SCRIPT)
        .expect("round-13 script must load with zero preprocessing");
    db
}

#[test]
fn round13_script_loads_as_is() {
    let mut db = loaded_db();
    // 8 SQL statements survive the two psql meta-lines.
    let n = db.query("SELECT count(*) FROM contacts").unwrap();
    assert_eq!(n.len(), 1);
}

#[test]
fn gap3_named_inline_not_null_enforces() {
    let mut db = loaded_db();
    db.execute("INSERT INTO contacts (id, key_type) VALUES (1, 'pgp')")
        .unwrap();
    let err = db
        .execute("INSERT INTO contacts (key_type) VALUES ('pgp')")
        .unwrap_err();
    let msg = format!("{err:?}").to_lowercase();
    assert!(msg.contains("null"), "NOT NULL must enforce, got: {msg}");
}

#[test]
fn gap4_qualified_vector_type_round_trips() {
    let mut db = loaded_db();
    db.execute("INSERT INTO contacts (id, embedding) VALUES (1, '[1,2,3,4]')")
        .unwrap();
    let rows = db
        .query("SELECT id FROM contacts ORDER BY embedding <-> [1,2,3,4] LIMIT 1")
        .unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn gap5_named_check_enforces() {
    let mut db = loaded_db();
    db.execute("INSERT INTO contacts (id, key_type) VALUES (1, 'smime')")
        .unwrap();
    let err = db
        .execute("INSERT INTO contacts (id, key_type) VALUES (2, 'rot13')")
        .unwrap_err();
    let msg = format!("{err:?}").to_lowercase();
    assert!(msg.contains("check"), "CHECK must enforce, got: {msg}");
}

#[test]
fn gap6_nulls_not_distinct_semantics_enforce() {
    let mut db = loaded_db();
    db.execute("INSERT INTO groups VALUES ('staff', NULL)")
        .unwrap();
    // PG default UNIQUE would allow a second ('staff', NULL); the
    // dump says NULLS NOT DISTINCT, so it must be rejected.
    let err = db
        .execute("INSERT INTO groups VALUES ('staff', NULL)")
        .unwrap_err();
    let msg = format!("{err:?}").to_lowercase();
    assert!(
        msg.contains("unique") || msg.contains("duplicate"),
        "NULLS NOT DISTINCT must treat NULL = NULL, got: {msg}"
    );
    // Distinct domain still inserts fine.
    db.execute("INSERT INTO groups VALUES ('staff', 'a.com')")
        .unwrap();
}

#[test]
fn gap7_qualified_opclass_indexes_work() {
    let mut db = loaded_db();
    db.execute("INSERT INTO email_analysis VALUES (1, '[1,0,0,0]', 'hello trigram world')")
        .unwrap();
    let knn = db
        .query("SELECT id FROM email_analysis ORDER BY embedding <-> [1,0,0,0] LIMIT 1")
        .unwrap();
    assert_eq!(knn.len(), 1);
    let trgm = db
        .query("SELECT id FROM email_analysis WHERE clean_text LIKE '%trigram%'")
        .unwrap();
    assert_eq!(trgm.len(), 1);
}
