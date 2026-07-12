//! v7.38 (read01 P3.25) — a pathologically nested expression errors with
//! a stack-depth message (like PG's check_stack_depth) instead of aborting
//! the process with a native stack overflow.

use spg_engine::{Engine, QueryResult};

#[test]
fn deeply_nested_expression_errors_instead_of_crashing() {
    let mut e = Engine::new();
    // Ordinary shallow nesting still evaluates.
    assert!(matches!(
        e.execute("SELECT 1 = 1 AND 2 = 2 AND 3 = 3").unwrap(),
        QueryResult::Rows { .. }
    ));

    // A deep-but-legal chain stays under the parser's chained-operator
    // budget (256) and simply evaluates. (Pre-1.97 toolchains had
    // eval_expr frames big enough that 150 frames crossed the eval
    // guard's 768 KiB budget; today they don't — the guard is
    // exercised parser-independently in eval.rs unit tests.)
    let ok_and = format!("SELECT {}", "1 = 1 AND ".repeat(150) + "1 = 1");
    assert!(matches!(
        e.execute(&ok_and).unwrap(),
        QueryResult::Rows { .. }
    ));

    // A pathological chain errors GRACEFULLY (the P3.25 regression was
    // a SIGABRT via native stack overflow): the parser's
    // chained-operator budget rejects it with a clean parse error.
    let big_and = format!("SELECT {}", "1 = 1 AND ".repeat(1000) + "1 = 1");
    let err = e.execute(&big_and).unwrap_err();
    assert!(
        format!("{err:?}").contains("chained binary operators"),
        "expected a graceful nesting error, got {err:?}"
    );

    // Deeply nested arithmetic is rejected the same way.
    let big_arith = format!("SELECT {}", "1 + ".repeat(1000) + "1");
    assert!(e.execute(&big_arith).is_err());

    // The engine is still usable afterwards (the guard unwinds cleanly).
    assert!(matches!(
        e.execute("SELECT 42").unwrap(),
        QueryResult::Rows { .. }
    ));
}
