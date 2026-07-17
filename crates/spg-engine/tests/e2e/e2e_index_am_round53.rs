//! v7.39 (read01 access/, round 53) — index access-method introspection.
//! pg_am listed only heap + btree, pg_class emitted no rows for indexes at
//! all (so PG's canonical
//! `pg_class JOIN pg_index ON indexrelid = oid JOIN pg_am ON relam = am.oid`
//! join — the one psql \d and every ORM use to learn an index's AM — came
//! back empty), and a `::regclass` cast inside a JOIN's pushed-down WHERE
//! lost the catalog and degraded to text.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn pairs(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                format!(
                    "{}|{}",
                    spg_engine::eval::value_to_text(&r.values[0]),
                    spg_engine::eval::value_to_text(&r.values[1])
                )
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn pg_am_lists_every_access_method() {
    let mut e = Engine::new();
    assert_eq!(
        col(
            &mut e,
            "SELECT amname FROM pg_am WHERE amname IN \
             ('btree','hash','gin','gist','brin') ORDER BY amname"
        ),
        vec!["brin", "btree", "gin", "gist", "hash"]
    );
}

#[test]
fn pg_class_emits_index_rows() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE gx(a int, j jsonb)");
    ok(&mut e, "CREATE INDEX gx_a ON gx(a)");
    ok(&mut e, "CREATE INDEX gx_j ON gx USING gin (j)");
    assert_eq!(
        col(
            &mut e,
            "SELECT relname FROM pg_class WHERE relkind='i' ORDER BY relname"
        ),
        vec!["gx_a", "gx_j"]
    );
}

#[test]
fn canonical_am_join_resolves() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE gx(a int, j jsonb)");
    ok(&mut e, "CREATE INDEX gx_a ON gx(a)");
    ok(&mut e, "CREATE INDEX gx_j ON gx USING gin (j)");
    // PG's canonical shape, including a ::regclass cast in a JOIN's WHERE —
    // the exact query psql \d and ORM introspection issue.
    assert_eq!(
        pairs(
            &mut e,
            "SELECT i.relname, am.amname FROM pg_class i \
             JOIN pg_index x ON x.indexrelid = i.oid \
             JOIN pg_am am ON am.oid = i.relam \
             WHERE x.indrelid = 'gx'::regclass ORDER BY i.relname"
        ),
        vec!["gx_a|btree", "gx_j|gin"]
    );
}

#[test]
fn regclass_cast_survives_a_join_pushdown() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE gx(a int)");
    ok(&mut e, "CREATE INDEX gx_a ON gx(a)");
    // The regression on its own: a WHERE conjunct that only references the
    // JOIN peer is pushed into the peer's scan, whose context used to lack
    // the catalog — so 'gx'::regclass stayed text and the comparison against
    // the BigInt indrelid raised a type mismatch.
    assert_eq!(
        col(
            &mut e,
            "SELECT count(*) FROM pg_class i JOIN pg_index x \
             ON x.indexrelid = i.oid WHERE x.indrelid = 'gx'::regclass"
        ),
        vec!["1"]
    );
}
