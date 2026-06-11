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

/// T2 — default-format pg_dump: COPY data blocks + the serial /
/// identity spellings (plain integer column + ALTER). Verbatim
/// pg_dump shapes.
const ROUND13_T2_SCRIPT: &str = "
CREATE TABLE public.messages (
    id bigint NOT NULL,
    subject text,
    score integer
);
CREATE SEQUENCE public.messages_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.messages_id_seq OWNED BY public.messages.id;
ALTER TABLE ONLY public.messages ALTER COLUMN id SET DEFAULT nextval('public.messages_id_seq'::regclass);
CREATE TABLE public.events (
    id bigint NOT NULL,
    kind text
);
ALTER TABLE public.events ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.events_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);
COPY public.messages (id, subject, score) FROM stdin;
1\tre: hello; world\t5
2\tit's a tab\\there\t\\N
\\.
SELECT pg_catalog.setval('public.messages_id_seq', 2, true);
COPY public.events (id, kind) FROM stdin;
\\.
";

#[test]
fn t2_copy_blocks_and_serial_spellings_import() {
    let mut db = Database::open_in_memory();
    db.execute_script(ROUND13_T2_SCRIPT)
        .expect("T2 dump script must load with zero preprocessing");
    // COPY data landed, with `;`, escaped tab and \N decoded.
    let rows = db
        .query("SELECT subject FROM messages ORDER BY id")
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0][0],
        spg_storage::Value::Text("re: hello; world".into())
    );
    assert_eq!(
        rows[1][0],
        spg_storage::Value::Text("it's a tab\there".into())
    );
    let null_score = db
        .query("SELECT id FROM messages WHERE score IS NULL")
        .unwrap();
    assert_eq!(null_score.len(), 1);
}

#[test]
fn t2_serial_keeps_numbering_after_import() {
    // THE post-cutover shape: the app inserts without an id. The
    // old no-op lowering of `SET DEFAULT nextval(…)` stripped
    // auto-increment from imported schemas and this INSERT died on
    // NOT NULL.
    let mut db = Database::open_in_memory();
    db.execute_script(ROUND13_T2_SCRIPT).unwrap();
    db.execute("INSERT INTO messages (subject) VALUES ('fresh')")
        .unwrap();
    let rows = db
        .query("SELECT id FROM messages WHERE subject = 'fresh'")
        .unwrap();
    assert_eq!(
        rows[0][0],
        spg_storage::Value::BigInt(3),
        "imported serial column must continue numbering at max+1"
    );
    // Identity spelling too (empty table starts at 1).
    db.execute("INSERT INTO events (kind) VALUES ('signup')")
        .unwrap();
    let ev = db.query("SELECT id FROM events").unwrap();
    assert_eq!(ev[0][0], spg_storage::Value::BigInt(1));
}

#[test]
fn t2_inline_identity_column() {
    let mut db = Database::open_in_memory();
    db.execute(
        "CREATE TABLE t (id bigint GENERATED BY DEFAULT AS IDENTITY (START WITH 1), x text)",
    )
    .unwrap();
    db.execute("INSERT INTO t (x) VALUES ('a')").unwrap();
    db.execute("INSERT INTO t (x) VALUES ('b')").unwrap();
    let rows = db.query("SELECT id FROM t ORDER BY id").unwrap();
    assert_eq!(rows[1][0], spg_storage::Value::BigInt(2));
    // Generated EXPRESSION columns reject loudly.
    let err = db
        .execute("CREATE TABLE g (a int, b int GENERATED ALWAYS AS (a * 2) STORED)")
        .unwrap_err();
    assert!(format!("{err:?}").contains("not supported"));
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
