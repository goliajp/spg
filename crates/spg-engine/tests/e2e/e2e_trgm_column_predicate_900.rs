//! 9.0.0 — pg_trgm's `%` worked in a select list and not in a WHERE.
//!
//! `SELECT id FROM t WHERE t.txt % 'needle'` answered
//! `operator does not exist: text % text` while
//! `SELECT txt % 'needle' FROM t` answered the similarity test. A
//! compiled predicate never passes through `eval_expr`'s `Expr::Binary`
//! arm, so the operator's type-based resolution — which lives there —
//! was out of reach. The same VM had the same hole for the MySQL
//! reading of `AND` / `OR`, found the same way.
//!
//! The corpus found this one: a select-list pin would have passed.

use spg_engine::{Engine, QueryResult};

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    rows.into_iter()
        .map(|r| spg_engine::eval::value_to_text(r.values.first().expect("one column")))
        .collect()
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE EXTENSION pg_trgm",
        "CREATE TABLE tp (id int, t text)",
        "INSERT INTO tp VALUES (1,'pad1 foo-bar tail'),(2,'pad2 foo-bar tail'),(3,'nothing alike')",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    e
}

#[test]
fn the_similarity_operator_works_as_a_predicate() {
    let mut e = seeded();
    assert_eq!(
        col(
            &mut e,
            "SELECT id FROM tp WHERE t % 'pad1 foo-bar tail' ORDER BY id"
        ),
        vec!["1".to_string(), "2".to_string()]
    );
    // The distance operator reaches the same path. The literal carries
    // an explicit `::real` because comparing a real-valued EXPRESSION to
    // a bare decimal literal inside a WHERE is refused here for reasons
    // that have nothing to do with pg_trgm — `abs(r) < 0.5` refuses the
    // same way, and the same comparison in a select list answers. That
    // is its own ledger row; this pin must not depend on it.
    assert_eq!(
        col(
            &mut e,
            "SELECT id FROM tp WHERE (t <-> 'pad1 foo-bar tail') < 0.5::real ORDER BY id"
        ),
        vec!["1".to_string(), "2".to_string()]
    );
}

#[test]
fn the_select_list_and_the_predicate_agree() {
    // The two paths must answer the same question — the select-list one
    // was right all along, which is exactly why the gap survived.
    let mut e = seeded();
    let listed = col(
        &mut e,
        "SELECT (t % 'pad1 foo-bar tail')::text FROM tp ORDER BY id",
    );
    assert_eq!(
        listed,
        vec!["true".to_string(), "true".to_string(), "false".to_string()]
    );
}
