//! v7.17.0 Phase 4.2 — MySQL CREATE PROCEDURE no-op (with @var
//! session variables in the body).

use spg_engine::Engine;

#[test]
fn create_procedure_simple_body_parses_as_empty() {
    let mut e = Engine::new();
    e.execute("CREATE PROCEDURE noop() BEGIN END").unwrap();
}

#[test]
fn procedure_with_session_var_assignment_parses() {
    let mut e = Engine::new();
    // mysqldump emits SET @var = ... inside procedure bodies.
    e.execute(
        "CREATE PROCEDURE foo() BEGIN \
            SET @x = 1; \
            SET @y = @x + 1; \
         END",
    )
    .unwrap();
}

#[test]
fn procedure_with_internal_semicolons() {
    let mut e = Engine::new();
    e.execute(
        "CREATE PROCEDURE bar() BEGIN \
            SET @counter = 0; \
            SET @counter = @counter + 1; \
            SET @counter = @counter + 1; \
         END",
    )
    .unwrap();
}

#[test]
fn procedure_with_nested_begin_end() {
    let mut e = Engine::new();
    e.execute(
        "CREATE PROCEDURE nested() BEGIN \
            SET @outer = 1; \
            BEGIN \
                SET @inner = 2; \
            END; \
            SET @after = 3; \
         END",
    )
    .unwrap();
}

#[test]
fn procedure_with_if_end_if() {
    let mut e = Engine::new();
    // `END IF` is part of the inner IF block — outer END
    // should still be the routine close.
    e.execute(
        "CREATE PROCEDURE conditional() BEGIN \
            SET @x = 1; \
            IF @x > 0 THEN \
                SET @y = 1; \
            END IF; \
            SET @z = 1; \
         END",
    )
    .unwrap();
}

#[test]
fn procedure_does_not_affect_subsequent_statements() {
    let mut e = Engine::new();
    e.execute("CREATE PROCEDURE p() BEGIN SET @x = 1; END")
        .unwrap();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
}

#[test]
fn procedure_with_args_and_trailing_semicolon() {
    let mut e = Engine::new();
    e.execute(
        "CREATE PROCEDURE add_one(IN val INT) BEGIN \
            SET @result = val + 1; \
         END;",
    )
    .unwrap();
}
