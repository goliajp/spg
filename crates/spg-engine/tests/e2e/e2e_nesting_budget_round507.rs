//! v7.39 (round 507) — every recursive SQL shape must reach a catchable
//! error, never the end of the stack.
//!
//! `MAX_NEST_DEPTH` exists for one reason: a stack overflow is an ABORT. It
//! does not fail one query — in the server it takes the process down and
//! every other connection with it, and an embed host cannot catch it. That
//! guarantee only holds for shapes the budget actually counts, and the one
//! test guarding it (`nesting_budget_errors_cleanly`) covered exactly one
//! shape: nested parentheses, which is the CHEAPEST of them.
//!
//! r506 found the reason nothing else was covered. Nested derived tables
//! cost roughly 235 KiB of stack per level in a debug build, so an ordinary
//! test thread aborts around 8 levels — long before the budget can fire.
//! A test that wants to reach the budget has to ask for a stack of its own.
//! It does that here, and the sweep is what the guarantee was missing.
//!
//! Measured against a live server first, on a 2 MiB worker stack in a
//! RELEASE build: every shape below returned an error or an answer, and the
//! server was still serving afterwards. That is the contract; this keeps it.

use spg_engine::{Engine, QueryResult};

/// Debug frames for the expensive shapes are enormous — see the header.
/// 64 MiB is roughly 4× what parsing to the 64-level budget needs.
const BIG_STACK: usize = 64 * 1024 * 1024;

/// What running `sql` did, without ever aborting the test binary.
enum Outcome {
    Answered,
    Refused(String),
}

fn run(sql: String) -> Outcome {
    std::thread::Builder::new()
        .stack_size(BIG_STACK)
        .spawn(move || {
            let mut e = Engine::new();
            e.execute("CREATE TABLE lbl (a INT, b INT)").unwrap();
            e.execute("INSERT INTO lbl VALUES (1, 10)").unwrap();
            match e.execute(&sql) {
                Ok(QueryResult::Rows { .. } | QueryResult::CommandOk { .. }) => Outcome::Answered,
                Ok(_) => Outcome::Answered,
                Err(err) => Outcome::Refused(format!("{err}")),
            }
        })
        .expect("spawn")
        .join()
        .expect("the parser must not overflow the stack — that is an abort")
}

fn nest(prefix: &str, inner: &str, suffix: &str, n: usize) -> String {
    format!("{}{inner}{}", prefix.repeat(n), suffix.repeat(n))
}

/// The shapes the nesting budget must catch. Each recurses one parser frame
/// chain per level, and each is well past `MAX_NEST_DEPTH`.
#[test]
fn round507_every_recursive_shape_reaches_the_nesting_budget() {
    let deep = 200;
    let cases = [
        ("derived", nest("SELECT * FROM (", "SELECT a FROM lbl", ") t", deep)),
        ("parens", format!("SELECT {}1{}", "(".repeat(deep), ")".repeat(deep))),
        ("calls", format!("SELECT {}'x'{}", "upper(".repeat(deep), ")".repeat(deep))),
        (
            "case",
            format!(
                "SELECT {}1{} FROM lbl",
                "CASE WHEN a=1 THEN ".repeat(deep),
                " ELSE 0 END".repeat(deep)
            ),
        ),
        (
            "in_subquery",
            nest("SELECT a FROM lbl WHERE a IN (", "SELECT a FROM lbl", ")", deep),
        ),
        (
            "scalar_subquery",
            nest("SELECT (", "SELECT a FROM lbl LIMIT 1", ")", deep),
        ),
        ("not", format!("SELECT {}TRUE", "NOT ".repeat(deep))),
        // Spaced: an unspaced run of minus signs is a LINE COMMENT, which
        // tests nothing. That mistake made a first pass of this sweep report
        // "survives a million levels".
        ("unary_minus", format!("SELECT {}1", "- ".repeat(deep))),
        ("unary_plus", format!("SELECT {}1", "+ ".repeat(deep))),
    ];
    for (name, sql) in cases {
        match run(sql) {
            Outcome::Refused(msg) => assert!(
                msg.contains("nests deeper"),
                "{name}: expected the nesting budget, got {msg}"
            ),
            Outcome::Answered => panic!("{name}: {deep} levels should not have been accepted"),
        }
    }
}

/// Shapes bounded by a DIFFERENT budget. They must still refuse rather than
/// run out of stack, and the wording says which budget caught them.
#[test]
fn round507_chained_shapes_reach_their_own_budgets() {
    let cases = [
        ("arith", format!("SELECT 1{}", " + 1".repeat(5_000)), "chained binary"),
        ("and", format!("SELECT a FROM lbl WHERE a=1{}", " AND a=1".repeat(5_000)), "chained binary"),
        ("concat", format!("SELECT 'x'{}", " || 'x'".repeat(5_000)), "chained binary"),
        ("cast", format!("SELECT 1{}", "::text".repeat(2_000)), "stack depth limit"),
    ];
    for (name, sql, want) in cases {
        match run(sql) {
            Outcome::Refused(msg) => {
                assert!(msg.contains(want), "{name}: expected {want:?}, got {msg}");
            }
            Outcome::Answered => panic!("{name}: should have been refused"),
        }
    }
}

/// Shapes that only LOOK recursive: they collect into a Vec, so depth costs
/// no stack and they are answered, not refused. Pinned so a future rewrite
/// that makes one of them recursive shows up here instead of as an abort.
#[test]
fn round507_flat_shapes_are_answered_not_refused() {
    let union = format!(
        "SELECT a FROM lbl{}",
        " UNION ALL SELECT a FROM lbl".repeat(2_000)
    );
    assert!(matches!(run(union), Outcome::Answered), "UNION chain");

    let mut cte = String::from("WITH c0 AS (SELECT a FROM lbl)");
    for i in 1..500 {
        cte.push_str(&format!(", c{i} AS (SELECT a FROM c{})", i - 1));
    }
    cte.push_str(" SELECT a FROM c499");
    assert!(matches!(run(cte), Outcome::Answered), "CTE chain");
}

/// Being refused must leave the engine usable — the budget is a rejection,
/// not a wound.
#[test]
fn round507_a_refused_statement_leaves_the_engine_working() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE lbl (a INT)").unwrap();
    e.execute("INSERT INTO lbl VALUES (1)").unwrap();
    let deep = format!("SELECT {}1{}", "(".repeat(200), ")".repeat(200));
    assert!(e.execute(&deep).is_err(), "must be refused");
    match e.execute("SELECT a FROM lbl").unwrap() {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("{other:?}"),
    }
}
