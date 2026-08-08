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

    // A deep-but-legal chain simply evaluates.
    //
    // How deep "legal" reaches is a property of the build, and both
    // limits were measured rather than assumed (round 850):
    //
    //   release  255 levels — capped by the parser's MAX_BINARY_CHAIN
    //                         of 256, so the eval guard never fires
    //   debug    100 levels — capped by the eval guard's 768 KiB, since
    //                         debug frames are around 2.5x wider
    //
    // 80 clears both. The number carries no product meaning: what this
    // file exists to prove is the pathological cases below erroring
    // rather than aborting, and PG parity on depth is the parser's
    // chain budget, not this line. An earlier 150 sat between the two
    // limits and started failing when a toolchain upgrade widened debug
    // frames — the guard was working correctly, and the assertion was
    // the thing that had assumed a build profile.
    let ok_and = format!("SELECT {}", "1 = 1 AND ".repeat(80) + "1 = 1");
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
