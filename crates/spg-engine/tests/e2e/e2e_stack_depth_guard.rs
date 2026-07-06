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

    // An AND chain that clears the parser's chained-operator limit but is
    // still deep enough to overflow the eval recursion (previously a
    // SIGABRT via native stack overflow) now returns a graceful error. The
    // depth that trips the guard depends on frame size (build profile);
    // 150 sits in the window that overflows a debug build's stack.
    let big_and = format!("SELECT {}", "1 = 1 AND ".repeat(150) + "1 = 1");
    let err = e.execute(&big_and).unwrap_err();
    assert!(
        format!("{err:?}").contains("StackDepthExceeded"),
        "expected a stack-depth error, got {err:?}"
    );

    // Deeply nested arithmetic trips the same guard.
    let big_arith = format!("SELECT {}", "1 + ".repeat(150) + "1");
    assert!(e.execute(&big_arith).is_err());

    // The engine is still usable afterwards (the guard unwinds cleanly).
    assert!(matches!(
        e.execute("SELECT 42").unwrap(),
        QueryResult::Rows { .. }
    ));
}
