//! v7.39 (read01 access/, round 52) — first pass over access/. Index
//! BEHAVIOUR was already right (ordering, NULLS FIRST/LAST, DESC, unique
//! NULLs-distinct, expression lookup, partial, multi-column, BETWEEN, IN,
//! min/max, IS NULL all matched PG with no change). Three gaps: pg_indexes
//! dropped the expression and the partial predicate from indexdef,
//! CREATE UNIQUE INDEX ... NULLS NOT DISTINCT didn't parse, and a failed
//! CREATE UNIQUE INDEX left the half-built index behind.

use spg_engine::{Engine, QueryResult};

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

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

fn seed(e: &mut Engine) {
    ok(e, "CREATE TABLE ix1(a int, b text, c int)");
    ok(
        e,
        "INSERT INTO ix1 VALUES (3,'c',1),(1,'a',NULL),(2,'b',2),(NULL,'z',3),(2,'bb',NULL)",
    );
}

#[test]
fn indexdef_keeps_expression_and_partial_predicate() {
    let mut e = Engine::new();
    seed(&mut e);
    ok(&mut e, "CREATE INDEX ix1_expr ON ix1(lower(b))");
    ok(&mut e, "CREATE INDEX ix1_part ON ix1(a) WHERE a > 1");
    let defs = col(
        &mut e,
        "SELECT indexdef FROM pg_indexes WHERE tablename='ix1' ORDER BY indexname",
    );
    assert!(
        defs.iter()
            .any(|d| d == "CREATE INDEX ix1_expr ON public.ix1 USING btree (lower(b))"),
        "{defs:?}"
    );
    assert!(
        defs.iter()
            .any(|d| d == "CREATE INDEX ix1_part ON public.ix1 USING btree (a) WHERE (a > 1)"),
        "{defs:?}"
    );
}

#[test]
fn unique_index_nulls_not_distinct() {
    let mut e = Engine::new();
    seed(&mut e);
    // Default NULLS DISTINCT: the two NULL `c` rows don't collide.
    ok(&mut e, "CREATE UNIQUE INDEX ix1_c ON ix1(c)");
    // NULLS NOT DISTINCT makes them collide, so the index can't be built.
    assert!(
        err(
            &mut e,
            "CREATE UNIQUE INDEX ix1_cn ON ix1(c) NULLS NOT DISTINCT"
        )
        .contains("could not create unique index \"ix1_cn\"")
    );
}

#[test]
fn failed_unique_index_is_rolled_back() {
    let mut e = Engine::new();
    seed(&mut e);
    // Two rows share a=2, so a unique index on it can't be built.
    assert!(
        err(&mut e, "CREATE UNIQUE INDEX ix1_a ON ix1(a)")
            .contains("could not create unique index \"ix1_a\"")
    );
    // …and it must not be left behind: PG's CREATE INDEX is atomic.
    assert!(
        col(
            &mut e,
            "SELECT indexname FROM pg_indexes WHERE tablename='ix1'"
        )
        .iter()
        .all(|n| n != "ix1_a")
    );
    // The name is therefore free to reuse.
    ok(&mut e, "CREATE INDEX ix1_a ON ix1(a)");
}

#[test]
fn nulls_not_distinct_enforced_on_insert() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE nd(a int)");
    ok(
        &mut e,
        "CREATE UNIQUE INDEX nd_a ON nd(a) NULLS NOT DISTINCT",
    );
    ok(&mut e, "INSERT INTO nd VALUES (NULL)");
    // Under NULLS NOT DISTINCT a second NULL is a duplicate.
    assert!(e.execute("INSERT INTO nd VALUES (NULL)").is_err());
    // A plain NULLS DISTINCT index would have allowed it.
    ok(&mut e, "CREATE TABLE nd2(a int)");
    ok(&mut e, "CREATE UNIQUE INDEX nd2_a ON nd2(a)");
    ok(&mut e, "INSERT INTO nd2 VALUES (NULL)");
    ok(&mut e, "INSERT INTO nd2 VALUES (NULL)");
}
