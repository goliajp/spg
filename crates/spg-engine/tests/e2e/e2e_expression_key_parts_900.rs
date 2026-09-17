//! 9.0.0 — an index key may mix columns and expressions in any position,
//! and an expression unique index is an `ON CONFLICT` arbiter.
//!
//! Every expectation below is PostgreSQL 18.6's answer to the same
//! statements, measured before the change:
//!
//!   * `UNIQUE (lower(a), b)` keyed on `lower(a)` alone, so after
//!     `('A', 1)` PG accepts `('a', 2)` and SPG refused it;
//!   * `(p, lower(email))` — an expression in a non-leading position —
//!     was a syntax error;
//!   * `ON CONFLICT (lower(email))` was a syntax error, and an untargeted
//!     `ON CONFLICT DO NOTHING` over an expression unique index raised the
//!     duplicate-key error PG absorbs (sentori's `cli/import.rs:36`).

use spg_engine::{Engine, QueryResult};

fn run(e: &mut Engine, sql: &str) -> Result<QueryResult, String> {
    e.execute(sql).map_err(|err| err.to_string())
}

fn ok(e: &mut Engine, sql: &str) -> QueryResult {
    run(e, sql).unwrap_or_else(|err| panic!("{sql}: {err}"))
}

fn affected(e: &mut Engine, sql: &str) -> usize {
    match ok(e, sql) {
        QueryResult::CommandOk { affected, .. } => affected,
        QueryResult::Rows { rows, .. } => rows.len(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn texts(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = ok(e, sql) else {
        panic!("{sql}: no rows");
    };
    rows.iter()
        .map(|r| {
            r.values
                .iter()
                .map(spg_engine::eval::value_to_text)
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

#[test]
fn a_unique_key_of_an_expression_and_a_column_reads_both_parts() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE ce (a text, b int)");
    ok(&mut e, "CREATE UNIQUE INDEX ce_u ON ce (lower(a), b)");
    ok(&mut e, "INSERT INTO ce VALUES ('A', 1)");
    let err = run(&mut e, "INSERT INTO ce VALUES ('a', 1)").expect_err("same key");
    assert!(
        err.contains("Key (lower(a), b)=(a, 1) already exists"),
        "PG names both parts: {err}"
    );
    ok(&mut e, "INSERT INTO ce VALUES ('a', 2)");
    assert_eq!(texts(&mut e, "SELECT count(*) FROM ce"), vec!["2"]);
}

#[test]
fn an_expression_may_stand_in_any_position_of_a_key() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE u2 (p int, email text)");
    ok(
        &mut e,
        "CREATE UNIQUE INDEX u2_pe ON u2 (p, lower(email) text_pattern_ops DESC)",
    );
    ok(&mut e, "INSERT INTO u2 VALUES (1, 'A')");
    let err = run(&mut e, "INSERT INTO u2 VALUES (1, 'a')").expect_err("same key");
    assert!(err.contains("Key (p, lower(email))=(1, a)"), "{err}");
    ok(&mut e, "INSERT INTO u2 VALUES (2, 'a')");
    assert_eq!(
        texts(
            &mut e,
            "SELECT indexdef FROM pg_indexes WHERE indexname = 'u2_pe'"
        ),
        vec![
            "CREATE UNIQUE INDEX u2_pe ON public.u2 USING btree (p, lower(email) text_pattern_ops DESC)"
        ]
    );
}

fn users() -> Engine {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE u1 (id int, email text, n int)");
    ok(&mut e, "CREATE UNIQUE INDEX u1_ci ON u1 (lower(email))");
    ok(&mut e, "INSERT INTO u1 VALUES (1, 'A@b.com', 1)");
    e
}

#[test]
fn an_expression_target_arbitrates_on_the_expression_index() {
    let mut e = users();
    assert_eq!(
        affected(
            &mut e,
            "INSERT INTO u1 VALUES (2, 'a@B.com', 2) ON CONFLICT (lower(email)) DO NOTHING"
        ),
        0
    );
    ok(
        &mut e,
        "INSERT INTO u1 VALUES (3, 'a@B.com', 3) ON CONFLICT (lower(email)) \
         DO UPDATE SET n = EXCLUDED.n",
    );
    assert_eq!(
        texts(&mut e, "SELECT id, email, n FROM u1"),
        vec!["1|A@b.com|3"]
    );
    // Spelled with extra parentheses, and with the default operator class.
    assert_eq!(
        affected(
            &mut e,
            "INSERT INTO u1 VALUES (4, 'a@b.com', 4) ON CONFLICT ((lower(email))) DO NOTHING"
        ),
        0
    );
    assert_eq!(
        affected(
            &mut e,
            "INSERT INTO u1 VALUES (5, 'a@b.com', 5) ON CONFLICT (lower(email) text_ops) DO NOTHING"
        ),
        0
    );
}

#[test]
fn an_untargeted_do_nothing_absorbs_an_expression_index_conflict() {
    let mut e = users();
    assert_eq!(
        affected(
            &mut e,
            "INSERT INTO u1 VALUES (2, 'A@B.COM', 4) ON CONFLICT DO NOTHING"
        ),
        0
    );
    assert_eq!(texts(&mut e, "SELECT count(*) FROM u1"), vec!["1"]);
}

#[test]
fn a_target_no_index_infers_is_refused() {
    let mut e = users();
    for sql in [
        "INSERT INTO u1 VALUES (6, 'x', 6) ON CONFLICT (upper(email)) DO NOTHING",
        "INSERT INTO u1 VALUES (6, 'a@b.com', 6) ON CONFLICT (lower(email) COLLATE \"C\") DO NOTHING",
    ] {
        let err = run(&mut e, sql).expect_err(sql);
        assert!(
            err.contains(
                "there is no unique or exclusion constraint matching the ON CONFLICT specification"
            ),
            "{sql}: {err}"
        );
    }
}

#[test]
fn a_mixed_target_is_matched_in_any_order() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE u2 (p int, email text)");
    ok(&mut e, "CREATE UNIQUE INDEX u2_pe ON u2 (p, lower(email))");
    ok(&mut e, "INSERT INTO u2 VALUES (1, 'A')");
    for sql in [
        "INSERT INTO u2 VALUES (1, 'a') ON CONFLICT (p, lower(email)) DO NOTHING",
        "INSERT INTO u2 VALUES (1, 'a') ON CONFLICT (lower(email), p) DO NOTHING",
    ] {
        assert_eq!(affected(&mut e, sql), 0, "{sql}");
    }
}

#[test]
fn a_partial_expression_index_is_inferred_only_by_its_predicate() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE u3 (email text, active bool)");
    ok(
        &mut e,
        "CREATE UNIQUE INDEX u3_pa ON u3 (lower(email)) WHERE active",
    );
    ok(&mut e, "INSERT INTO u3 VALUES ('A', true)");
    assert_eq!(
        affected(
            &mut e,
            "INSERT INTO u3 VALUES ('a', true) ON CONFLICT (lower(email)) WHERE active DO NOTHING"
        ),
        0
    );
    let err = run(
        &mut e,
        "INSERT INTO u3 VALUES ('a', true) ON CONFLICT (lower(email)) DO NOTHING",
    )
    .expect_err("no WHERE");
    assert!(err.contains("no unique or exclusion constraint"), "{err}");
}

#[test]
fn a_copy_of_the_table_carries_the_whole_index() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE src (p int, email text)");
    ok(
        &mut e,
        "CREATE UNIQUE INDEX src_pe ON src (p, lower(email) DESC) WHERE p > 0",
    );
    ok(&mut e, "CREATE TABLE dst (LIKE src INCLUDING INDEXES)");
    assert_eq!(
        texts(
            &mut e,
            "SELECT indexdef FROM pg_indexes WHERE tablename = 'dst'"
        ),
        vec![
            "CREATE UNIQUE INDEX dst_p_lower_idx ON public.dst USING btree (p, lower(email) DESC) WHERE (p > 0)"
        ]
    );
}

#[test]
fn the_key_parts_survive_a_reload() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE ce (a text, b int)");
    ok(&mut e, "CREATE UNIQUE INDEX ce_u ON ce (b, lower(a))");
    ok(&mut e, "INSERT INTO ce VALUES ('A', 1)");
    let bytes = e.catalog().serialize();
    let mut r = Engine::restore_envelope(&bytes).expect("reload");
    let err = run(&mut r, "INSERT INTO ce VALUES ('a', 1)").expect_err("same key");
    assert!(err.contains("Key (b, lower(a))=(1, a)"), "{err}");
    ok(&mut r, "INSERT INTO ce VALUES ('a', 2)");
}

/// A data directory from before 9.0.0 recorded `(a DESC)` as an expression
/// index on `a`. Read back, the part is the column again: `indkey` names
/// the column, as PG's does.
#[test]
fn an_old_bare_column_key_recorded_as_an_expression_reads_back_as_the_column() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE old (a int, b int)");
    ok(&mut e, "CREATE INDEX old_ab ON old (a DESC, b)");
    let mut cat = spg_storage::Catalog::deserialize(&e.catalog().serialize()).expect("image");
    let idx = cat
        .get_mut("old")
        .expect("table")
        .indices_mut()
        .iter_mut()
        .find(|i| i.name == "old_ab")
        .expect("index");
    idx.expression = Some("a".into());
    let mut r = Engine::restore_envelope(&cat.serialize()).expect("reload");
    assert_eq!(
        texts(
            &mut r,
            "SELECT indkey::text, coalesce(indexprs, '-') FROM pg_index \
             WHERE indexrelid = 'old_ab'::regclass"
        ),
        vec!["1 2|-"]
    );
}
