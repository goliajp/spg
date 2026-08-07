//! v7.39 (round 621) — `AND` and `OR` evaluated both sides, always, so the
//! commonest guard in SQL did not work.
//!
//! `WHERE x <> 0 AND 1/x > 0` raised `division by zero` on exactly the rows
//! the guard exists to exclude. That predicate is written that way FOR the
//! guard; a database that evaluates the right side anyway has taken the one
//! thing it does and removed it. PG answers it, and answers `f` for
//! `false AND (1/0 = 0)` and `t` for `true OR (1/0 = 0)`.
//!
//! What made this more than reordering two lines is that PG ALSO refuses
//! `false AND 1` — a short circuit that reached the answer would never see the
//! `1`. The two coexist because they happen at different times: the operand
//! types are checked during ANALYSIS, before anything is evaluated, and the
//! short circuit is a RUN-TIME decision. Round 238 had put the type check in
//! front of the evaluation, which is the only place SPG had to put it, and
//! that is what stopped the guard working.
//!
//! Both halves are here. The analysis half reads the right-hand operand — the
//! side that may go unevaluated — and refuses it when it is a LITERAL that is
//! plainly not boolean, and resolves it when it is an unknown string literal
//! (so `false AND 'a'` is the input-syntax error PG raises, not `f`).
//!
//! Only a literal is read that way, and the first cut was wider and wrong: it
//! asked the describer for any expression's type, and the describer answers
//! confidently and incorrectly for shapes that matter here — `NULL` comes back
//! as `text`, so `true AND NULL` earned a type error, and a MATCH … AGAINST
//! folds internally into an `OR` over tsvector operands, so full-text search
//! stopped working entirely. Three existing pins caught all six failures. A
//! literal cannot be misread.
//!
//! Order is PG's and is strictly left-first: `(1/0 = 0) AND false` raises on
//! both, because the left is evaluated before anything can decide it did not
//! need to be. And a NULL left side decides nothing — `NULL AND (1/0 = 0)`
//! raises, because NULL AND false is false, so the right side is needed.
//!
//! 19 shapes were checked against live PG18 and 18 are byte-identical. The
//! one that is not: `false AND (SELECT 1/0)` — PG types the subquery during
//! analysis and refuses the operand, SPG raises the division, because a
//! planned scalar subquery is spliced into the tree BEFORE this evaluation
//! sees it. Both error; the wording differs. Recorded, not faked.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).expect_err(sql))
}

/// The guard, which is the point.
#[test]
fn round621_a_guard_guards() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE gd (x INT)").unwrap();
    e.execute("INSERT INTO gd VALUES (1),(0),(2)").unwrap();
    assert_eq!(
        vals(
            &mut e,
            "SELECT x FROM gd WHERE x <> 0 AND 10/x > 1 ORDER BY 1"
        ),
        vec!["1", "2"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT x FROM gd WHERE x = 0 OR 10/x > 1 ORDER BY 1"
        ),
        vec!["0", "1", "2"],
        "the OR spelling of the same guard"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT x FROM gd WHERE NOT (x = 0) AND 1/x > 0 ORDER BY 1"
        ),
        vec!["1"],
        "the guard behind a NOT. Only row 1: `1/2` is INTEGER division and is \
         0, so row 2 fails the predicate on its merits — this expectation was \
         hand-computed as [1, 2] first and the test said otherwise"
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM gd WHERE x <> 0 AND 10/x > 1"),
        vec!["2"]
    );
}

/// The decision, and who is allowed to make it.
#[test]
fn round621_the_left_side_decides_or_it_does_not() {
    let mut e = Engine::new();
    assert_eq!(vals(&mut e, "SELECT false AND (1/0 = 0)"), vec!["false"]);
    assert_eq!(vals(&mut e, "SELECT true OR (1/0 = 0)"), vec!["true"]);
    assert_eq!(
        vals(&mut e, "SELECT false AND (1/0 = 0) AND (2/0 = 0)"),
        vec!["false"],
        "and it keeps deciding down the chain"
    );
    assert_eq!(
        vals(&mut e, "SELECT (false AND (1/0=0)) OR true"),
        vec!["true"]
    );
    assert!(
        err(&mut e, "SELECT (1/0 = 0) AND false").contains("division by zero"),
        "strictly left-first: the left is evaluated before anything can decide \
         that it did not need to be"
    );
    assert!(err(&mut e, "SELECT (1/0 = 0) OR true").contains("division by zero"));
    assert!(
        err(&mut e, "SELECT NULL AND (1/0 = 0)").contains("division by zero"),
        "a NULL left side decides nothing — NULL AND false is false, so the \
         right side is needed"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT false AND NULL, true OR NULL, NULL AND false, NULL OR true"
        ),
        vec!["false|true|false|true"],
        "the three-valued rules are untouched"
    );
}

/// The analysis half: what is refused even though the short circuit would
/// never have looked at it.
#[test]
fn round621_the_operand_is_still_type_checked() {
    let mut e = Engine::new();
    assert!(
        err(&mut e, "SELECT false AND 1")
            .contains("argument of AND must be type boolean, not type integer"),
        "PG refuses this despite deciding on the left, because the type is \
         checked during analysis: {}",
        err(&mut e, "SELECT false AND 1")
    );
    assert!(
        err(&mut e, "SELECT true OR 1")
            .contains("argument of OR must be type boolean, not type integer")
    );
    assert!(
        err(&mut e, "SELECT 1 AND false")
            .contains("argument of AND must be type boolean, not type integer"),
        "and on the left, where it was already right"
    );
    assert!(
        err(&mut e, "SELECT false AND 'a'")
            .contains(r#"invalid input syntax for type boolean: "a""#),
        "resolving an unknown literal is part of the same half — it is a \
         coercion PG performs while analysing"
    );
    assert_eq!(
        vals(&mut e, "SELECT false AND 'true', true OR 'false'"),
        vec!["false|true"],
        "and one that resolves simply resolves"
    );
}

/// The shapes the first, over-wide cut of the analysis half broke. They are
/// pinned here because a describer-based type read is the obvious way to write
/// this and it is wrong.
#[test]
fn round621_what_must_not_be_type_read() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT true AND NULL"),
        vec!["NULL"],
        "NULL is not text, whatever the describer says about it"
    );
    assert_eq!(vals(&mut e, "SELECT (false AND NULL)::TEXT"), vec!["false"]);
    let mut e2 = Engine::new();
    e2.execute("CREATE TABLE bc (a BOOLEAN, b BOOLEAN)")
        .unwrap();
    e2.execute("INSERT INTO bc VALUES (true,false),(false,true)")
        .unwrap();
    assert_eq!(
        vals(&mut e2, "SELECT count(*) FROM bc WHERE a OR b"),
        vec!["2"],
        "two boolean COLUMNS, which the describer types fine but which must \
         not be pre-empted either"
    );
}
