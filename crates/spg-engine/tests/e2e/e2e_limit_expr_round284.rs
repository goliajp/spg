//! v7.39 (round 284) — `LIMIT` / `OFFSET` over an expression.
//!
//! PG's row-count clause takes a general expression, so `LIMIT 1+1` is
//! legal; SPG accepted only a single token and answered a parse error.
//! r239 had built the coercion rules (numeric rounding, string content,
//! negative refusal) and recorded the expression case as a residual —
//! this closes it for constant expressions.
//!
//! Constants are folded in the PARSER rather than carried into the tree.
//! That is not an optimisation: the 15+ execution paths that read the
//! row count go through `limit_literal()`, which answers `Option<u32>`,
//! and `None` there means "no limit". A clause the engine failed to
//! resolve would quietly return the WHOLE table. Folding at parse time
//! makes that unrepresentable.
//!
//! Every expectation was read off live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn count(e: &mut Engine, tail: &str) -> String {
    let sql = format!("SELECT count(*) FROM (SELECT * FROM l2 {tail}) s");
    let r = e
        .execute(&sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows from {sql}");
    };
    spg_engine::eval::value_to_text(&rows[0].values[0])
}

fn err(e: &mut Engine, tail: &str) -> String {
    let sql = format!("SELECT count(*) FROM (SELECT * FROM l2 {tail}) s");
    match e.execute(&sql) {
        Ok(v) => panic!("{sql}: expected an error, got {v:?}"),
        // v7.39 (round 323, V24) — the `parse error at token #N: ` wrapper
        // this used to unpick is gone; only the engine's own layer prefix
        // is left, and the message under it is PG's verbatim.
        Err(x) => format!("{x}").trim_start_matches("parse: ").to_string(),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE l2 (id int)").unwrap();
    e.execute("INSERT INTO l2 VALUES (1),(2),(3),(4),(5),(6),(7),(8),(9),(10)")
        .unwrap();
    e
}

#[test]
fn arithmetic_is_folded_to_a_row_count() {
    let mut e = fixture();
    assert_eq!(count(&mut e, "LIMIT 1+1"), "2");
    assert_eq!(count(&mut e, "LIMIT 2*3"), "6");
    assert_eq!(count(&mut e, "LIMIT (1+1)*2"), "4");
    assert_eq!(count(&mut e, "LIMIT 10/4"), "2");
    assert_eq!(count(&mut e, "LIMIT 10%3"), "1");
    assert_eq!(count(&mut e, "OFFSET 2+3"), "5");
    assert_eq!(count(&mut e, "LIMIT 3 OFFSET 1+1"), "3");
    assert_eq!(count(&mut e, "ORDER BY id DESC LIMIT 1+2"), "3");
}

#[test]
fn the_single_token_forms_are_unchanged() {
    // r239's coercion rules must survive the move to the expression
    // grammar — this is the half a rewrite would silently drop.
    let mut e = fixture();
    assert_eq!(count(&mut e, "LIMIT ALL"), "10");
    assert_eq!(count(&mut e, "LIMIT NULL"), "10");
    // numeric → bigint rounds half away from zero
    assert_eq!(count(&mut e, "LIMIT 2.7"), "3");
    assert_eq!(count(&mut e, "LIMIT 2.4"), "2");
    // a string coerces by its content
    assert_eq!(count(&mut e, "LIMIT '3'"), "3");
    assert_eq!(count(&mut e, "LIMIT 4 OFFSET 2"), "4");
    assert_eq!(count(&mut e, "FETCH FIRST 2 ROWS ONLY"), "2");
}

#[test]
fn a_negative_result_is_refused_after_folding() {
    let mut e = fixture();
    assert_eq!(err(&mut e, "LIMIT -1"), "LIMIT must not be negative");
    assert_eq!(err(&mut e, "OFFSET -1"), "OFFSET must not be negative");
    // The fold happens FIRST — 3-5 is not negative until evaluated.
    assert_eq!(err(&mut e, "LIMIT 3-5"), "LIMIT must not be negative");
}

#[test]
fn the_type_and_overflow_wordings_are_pgs() {
    let mut e = fixture();
    assert_eq!(
        err(&mut e, "LIMIT 'abc'"),
        "invalid input syntax for type bigint: \"abc\"",
    );
    assert_eq!(
        err(&mut e, "OFFSET 'x'"),
        "invalid input syntax for type bigint: \"x\"",
    );
    // PG names the clause in the type error — LIMIT and OFFSET differ.
    assert_eq!(
        err(&mut e, "LIMIT true"),
        "argument of LIMIT must be type bigint, not type boolean",
    );
    assert_eq!(
        err(&mut e, "OFFSET true"),
        "argument of OFFSET must be type bigint, not type boolean",
    );
    // The arithmetic overflows in int before the row count is looked at.
    assert_eq!(err(&mut e, "LIMIT 2000000000*2"), "integer out of range");
}

#[test]
fn fetch_first_keeps_pgs_constant_only_grammar() {
    // PG answers `syntax error at or near "+"` here: FETCH FIRST takes
    // a constant or a PARENTHESISED expression, never bare arithmetic.
    // (Round 305 measured the other half — `FETCH FIRST (1+1) ROWS ONLY`
    // and `FETCH FIRST (SELECT 3) ROWS ONLY` are both legal there, and
    // are pinned in that round's file.)
    let mut e = fixture();
    assert!(
        e.execute("SELECT * FROM l2 FETCH FIRST 1+1 ROWS ONLY")
            .is_err()
    );
}

#[test]
fn a_non_constant_clause_never_quietly_widens_to_the_whole_table() {
    // The hazard round 284 had to avoid: `limit_literal()` answers
    // Option<u32>, and None means "no limit", so a clause the parser
    // cannot fold must never reach execution unresolved. Round 284 met
    // that by refusing such a clause; round 305 met it properly, by
    // evaluating it before dispatch (V23). Restated against PG rather
    // than deleted — what must hold either way is that the row count
    // takes effect, and the one shape PG itself refuses still fails.
    let mut e = fixture();
    for (tail, want) in [
        ("LIMIT (SELECT 4)", 4),
        ("LIMIT greatest(2,3)", 3),
        ("OFFSET (SELECT 1)", 9),
    ] {
        let sql = format!("SELECT * FROM l2 ORDER BY id {tail}");
        match e.execute(&sql).unwrap_or_else(|x| panic!("{tail}: {x:?}")) {
            spg_engine::QueryResult::Rows { rows, .. } => {
                assert_eq!(rows.len(), want, "{tail} did not apply its row count");
            }
            other => panic!("{tail}: {other:?}"),
        }
    }
    // A column reference stays an error — the clause is evaluated once,
    // before the scan, so PG rejects it too.
    assert!(e.execute("SELECT * FROM l2 LIMIT id").is_err());
}
