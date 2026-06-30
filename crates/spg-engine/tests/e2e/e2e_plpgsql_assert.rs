//! v7.37.20 (20.14) — PL/pgSQL ASSERT statement.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn assert_true_inside_do_block_is_no_op() {
    let mut e = Engine::new();
    ddl(&mut e, "DO $$ BEGIN ASSERT 1 = 1; END $$;");
}

#[test]
fn assert_false_raises_with_default_message() {
    let mut e = Engine::new();
    let err = e.execute("DO $$ BEGIN ASSERT 1 = 2; END $$;");
    let err_msg = format!("{err:?}");
    assert!(err.is_err(), "expected ASSERT 1=2 to error: {err:?}");
    assert!(
        err_msg.contains("assertion failed"),
        "expected default assertion message, got: {err_msg}"
    );
}

#[test]
fn assert_false_with_custom_message_propagates_message() {
    let mut e = Engine::new();
    let err = e.execute("DO $$ BEGIN ASSERT 1 = 2, 'invariant X broken'; END $$;");
    let err_msg = format!("{err:?}");
    assert!(err.is_err(), "expected error");
    assert!(
        err_msg.contains("invariant X broken"),
        "expected custom message, got: {err_msg}"
    );
}

#[test]
fn assert_null_condition_treats_as_false() {
    let mut e = Engine::new();
    let err = e.execute("DO $$ BEGIN ASSERT NULL; END $$;");
    assert!(err.is_err(), "ASSERT NULL should fail");
}
