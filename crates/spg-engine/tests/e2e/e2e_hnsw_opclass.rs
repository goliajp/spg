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
    // r1038 — this used to pass by accident. The parser matched the
    // opclass against a whitelist of names, so garbage did not parse; that
    // whitelist was also why `USING gin (doc jsonb_path_ops)` — ordinary
    // PG — was a syntax error, and recognising an opclass by position
    // instead took the refusal away with it. The refusal now comes from
    // the catalog check, which is where PG's comes from too.
    //
    // Wording verified against PG18.4:
    //   ERROR:  operator class "weird_garbage" does not exist for access
    //           method "gin"
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (e VECTOR(4) NOT NULL)")
        .unwrap();
    let err = eng
        .execute("CREATE INDEX t_e ON t USING hnsw (e weird_garbage)")
        .unwrap_err();
    let msg = alloc_msg(&err);
    assert!(
        msg.contains(r#"operator class "weird_garbage" does not exist for access method "hnsw""#),
        "{msg}"
    );
}

/// The class has to exist for THE ACCESS METHOD NAMED, which is why the
/// name the user wrote is carried past the point where `gist` / `spgist` /
/// `hash` all become a BTree. PG18.4 refuses this one too:
/// `operator class "int4_ops" does not exist for access method "gin"`.
#[test]
fn opclass_of_the_wrong_access_method_is_refused() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, j JSONB)")
        .unwrap();
    let err = eng
        .execute("CREATE INDEX t_j ON t USING gin (j int4_ops)")
        .unwrap_err();
    let msg = alloc_msg(&err);
    assert!(
        msg.contains(r#"operator class "int4_ops" does not exist for access method "gin""#),
        "{msg}"
    );
    // …and the same class under btree, where it does exist, is fine.
    eng.execute("CREATE INDEX t_id ON t (id int4_ops)").unwrap();
}

/// `USING gist` degrades to a BTree so PG schemas load. The opclass is
/// still checked against GiST's list, not the BTree it became — otherwise
/// a GiST-only class would be refused for an access method the user never
/// named.
#[test]
fn degraded_access_methods_keep_their_own_opclass_list() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, c TEXT)")
        .unwrap();
    // gist_trgm_ops is GiST's and not btree's.
    eng.execute("CREATE INDEX t_c ON t USING gist (c gist_trgm_ops)")
        .unwrap();
    // int4_ops is btree's and not GiST's — PG18.4 refuses it under gist.
    let err = eng
        .execute("CREATE INDEX t_id ON t USING gist (id int4_ops)")
        .unwrap_err();
    let msg = alloc_msg(&err);
    assert!(msg.contains(r#"for access method "gist""#), "{msg}");
}

/// PG18.4 accepts `(d date_ops DESC)`. The positional rule reads ASC /
/// DESC as their own tokens, not as identifiers spelled that way — the
/// first version matched `Token::Ident("desc")`, which the lexer never
/// produces, so the arm was dead and this shape still failed to parse.
///
/// `date_ops` on purpose: the names the OLD whitelist held
/// (`text_pattern_ops`, `int4_ops`, …) reach the opclass arm without the
/// positional rule at all, so a test written with one of those passes
/// whether or not the fix is there. That is how the first draft of this
/// test passed its own negative control.
#[test]
fn opclass_followed_by_a_sort_direction_parses() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (d DATE)").unwrap();
    eng.execute("CREATE INDEX t_d ON t (d date_ops DESC)")
        .unwrap();
    eng.execute("CREATE INDEX t_d2 ON t (d date_ops ASC)")
        .unwrap();
    eng.execute("CREATE INDEX t_d3 ON t (d date_ops NULLS FIRST)")
        .unwrap();
    // And with nothing after it, which is the shape that already worked.
    eng.execute("CREATE INDEX t_d4 ON t (d date_ops)").unwrap();
}

fn alloc_msg(e: &spg_engine::EngineError) -> String {
    format!("{e}")
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
