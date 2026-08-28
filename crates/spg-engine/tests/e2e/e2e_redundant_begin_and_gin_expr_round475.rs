//! read01 round 475 (C10 / C1) — a redundant BEGIN, and GIN on an expression.
//!
//! C10. SPG raised "a transaction is already open" for a second BEGIN AND
//! left the transaction aborted, so the next statement failed with "current
//! transaction is aborted" and the whole block was lost. A pooler or a
//! framework that wraps its own BEGIN around one the caller already opened
//! does this routinely.
//!
//! The two oracles genuinely differ, and both were measured:
//!   PG18        WARNING, the BEGIN is a no-op, the transaction continues —
//!               a later ROLLBACK undoes both inserts.
//!   MariaDB 11  START TRANSACTION implicitly COMMITS the open one, so the
//!               first insert survives the rollback and the second does not.
//!
//! C1. `CREATE INDEX … USING gin (to_tsvector('simple', doc))` — PG's
//! canonical full-text idiom — was refused, and the refusal came AFTER the
//! index had been built: the error said nothing happened while a btree index
//! named `gx` on `doc` existed, and a dump carried it. The message also
//! named HNSW and BRIN for a GIN index.

use spg_engine::{Engine, QueryResult};

fn scalar(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join(";"),
        other => panic!("{sql} -> {other:?}"),
    }
}

fn nested_begin_rows(mysql: bool) -> String {
    let mut e = Engine::new();
    if mysql {
        e.set_mysql_wire_session();
    }
    e.execute("CREATE TABLE t (a INT)").unwrap();
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    // The statement that used to raise and poison the block.
    e.execute("BEGIN")
        .expect("a redundant BEGIN must not fail the transaction");
    e.execute("INSERT INTO t VALUES (2)")
        .expect("the block must still be usable after a redundant BEGIN");
    e.execute("ROLLBACK").unwrap();
    scalar(&mut e, "SELECT count(*) FROM t")
}

#[test]
fn round475_pg_treats_a_redundant_begin_as_a_no_op() {
    // PG18 warns and keeps ONE transaction, so the rollback undoes both.
    assert_eq!(nested_begin_rows(false), "0");
}

#[test]
fn round475_mysql_implicitly_commits_on_a_nested_start() {
    // MariaDB 11 commits the open transaction first, so the first row lives.
    assert_eq!(nested_begin_rows(true), "1");
}

#[test]
fn round475_gin_on_to_tsvector_builds_a_fulltext_index() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE g (id INT, doc TEXT)").unwrap();
    e.execute("CREATE INDEX gx ON g USING gin (to_tsvector('simple', doc))")
        .expect("PG's canonical full-text idiom must build");
    // PG18: CREATE INDEX gx ON public.g USING gin (to_tsvector('simple'::regconfig, doc))
    // SPG renders the config literal without PG's ::regconfig annotation.
    assert_eq!(
        scalar(
            &mut e,
            "SELECT indexdef FROM pg_indexes WHERE tablename = 'g'"
        ),
        "CREATE INDEX gx ON public.g USING gin (to_tsvector('simple', doc))"
    );
}

#[test]
fn round475_a_refused_index_leaves_nothing_behind() {
    // The state corruption: the error used to arrive after the build.
    let mut e = Engine::new();
    e.execute("CREATE TABLE g (id INT, doc TEXT)").unwrap();
    let err = e
        .execute("CREATE INDEX gx ON g USING brin (lower(doc))")
        .expect_err("an expression key on BRIN is refused");
    let msg = format!("{err}");
    assert!(
        msg.contains("expression keys are not supported on BRIN indexes"),
        "the message must name the method it refused: {msg}"
    );
    assert_eq!(
        scalar(
            &mut e,
            "SELECT count(*) FROM pg_indexes WHERE tablename = 'g'"
        ),
        "0",
        "a refused CREATE INDEX must not leave an index behind"
    );
}

#[test]
fn round475_indexdef_reports_the_access_method_it_really_is() {
    // It said `btree` for every index, so a GIN index dumped as a btree.
    let mut e = Engine::new();
    e.execute("CREATE TABLE g (id INT, doc TEXT, v TSVECTOR)")
        .unwrap();
    e.execute("CREATE INDEX bx ON g (id)").unwrap();
    e.execute("CREATE INDEX vx ON g USING gin (v)").unwrap();
    assert_eq!(
        scalar(
            &mut e,
            "SELECT indexdef FROM pg_indexes WHERE tablename='g' AND indexname='bx'"
        ),
        "CREATE INDEX bx ON public.g USING btree (id)"
    );
    assert_eq!(
        scalar(
            &mut e,
            "SELECT indexdef FROM pg_indexes WHERE tablename='g' AND indexname='vx'"
        ),
        "CREATE INDEX vx ON public.g USING gin (v)"
    );
}
