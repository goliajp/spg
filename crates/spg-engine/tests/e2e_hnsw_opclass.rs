//! v7.9.22 — HNSW pgvector opclass syntax accept.
//! mailrs migration follow-up G5.

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
fn vector_cosine_ops_is_accepted_and_index_builds() {
    let mut eng = engine_with(&[
        "CREATE TABLE email_analysis (id INT NOT NULL, embedding VECTOR(8) NOT NULL)",
    ]);
    eng.execute(
        "CREATE INDEX idx_ea_embedding ON email_analysis \
         USING hnsw (embedding vector_cosine_ops)",
    )
    .unwrap();
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    assert!(
        cat.get("email_analysis")
            .unwrap()
            .indices()
            .iter()
            .any(|i| i.name == "idx_ea_embedding")
    );
}

#[test]
fn vector_l2_ops_accepted() {
    let mut eng = engine_with(&["CREATE TABLE docs (id INT NOT NULL, e VECTOR(4) NOT NULL)"]);
    eng.execute("CREATE INDEX docs_e ON docs USING hnsw (e vector_l2_ops)")
        .unwrap();
}

#[test]
fn vector_ip_ops_accepted() {
    let mut eng = engine_with(&["CREATE TABLE docs (id INT NOT NULL, e VECTOR(4) NOT NULL)"]);
    eng.execute("CREATE INDEX docs_e ON docs USING hnsw (e vector_ip_ops)")
        .unwrap();
}

#[test]
fn unknown_opclass_falls_through_to_existing_error() {
    // Garbage ident after the column should still be rejected;
    // we only accept the pgvector / SPG opclass whitelist.
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (e VECTOR(4) NOT NULL)")
        .unwrap();
    let r = eng.execute("CREATE INDEX t_e ON t USING hnsw (e weird_garbage)");
    assert!(r.is_err());
}

#[test]
fn opclass_after_existing_hnsw_index_round_trip() {
    let eng = engine_with(&[
        "CREATE TABLE docs (id INT NOT NULL, e VECTOR(8) NOT NULL)",
        "CREATE INDEX docs_e ON docs USING hnsw (e vector_cosine_ops)",
    ]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    assert!(
        cat.get("docs")
            .unwrap()
            .indices()
            .iter()
            .any(|i| i.name == "docs_e")
    );
}
