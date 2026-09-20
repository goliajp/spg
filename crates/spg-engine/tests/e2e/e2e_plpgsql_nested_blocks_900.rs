//! 9.0.0 — nested PL/pgSQL blocks and the labels that name them.
//!
//! `DO $$ BEGIN BEGIN NULL; END; END; $$` was a syntax error. Any
//! statement position in PL/pgSQL may hold a `[<<label>>] [DECLARE …]
//! BEGIN … END` block — it is how a body scopes a variable or catches
//! an exception around part of itself — and none of it parsed: with an
//! EXCEPTION clause, with a DECLARE prelude, or bare. A top-level
//! EXCEPTION handler did work, which is why the gap stayed hidden.
//!
//! Labels went with it. `<<lp>> FOR … END LOOP lp`, `EXIT <label>` and
//! `CONTINUE <label>` were all syntax errors, so a body could not leave
//! an outer loop from an inner one at all.
//!
//! Every expectation here was measured against PostgreSQL 18.6:
//!
//!   * an inner `DECLARE x` shadows the outer `x` for the block and the
//!     outer value is back afterwards; an assignment to a name the
//!     inner block did NOT declare reaches the outer one and survives;
//!   * an inner EXCEPTION catches and the outer block carries on;
//!   * `EXIT <label>` leaves the loop OR the block that carries it,
//!     `CONTINUE <label>` resumes that loop;
//!   * `END <label>` must agree with the block's, and an end label on
//!     an unlabelled block is an error — both of PG's sentences.

use spg_engine::{Engine, QueryResult};

fn notices(e: &mut Engine, sql: &str) -> Vec<String> {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    e.take_notices().into_iter().map(|n| n.message).collect()
}

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

#[test]
fn a_block_may_hold_another_block() {
    let mut e = Engine::new();
    assert_eq!(
        notices(
            &mut e,
            "DO $$ BEGIN BEGIN RAISE NOTICE 'nested'; END; END; $$"
        ),
        vec!["nested".to_string()]
    );
    // With a DECLARE prelude of its own.
    assert_eq!(
        notices(
            &mut e,
            "DO $$ DECLARE x int := 1; BEGIN DECLARE y int := 2; \
             BEGIN RAISE NOTICE '% %', x, y; END; END; $$"
        ),
        vec!["1 2".to_string()]
    );
}

#[test]
fn an_inner_declaration_shadows_and_an_inner_assignment_does_not() {
    let mut e = Engine::new();
    assert_eq!(
        notices(
            &mut e,
            "DO $$ DECLARE x int := 1; BEGIN \
               DECLARE x int := 2; BEGIN RAISE NOTICE 'inner %', x; END; \
               RAISE NOTICE 'outer %', x; END; $$"
        ),
        vec!["inner 2".to_string(), "outer 1".to_string()]
    );
    assert_eq!(
        notices(
            &mut e,
            "DO $$ DECLARE x int := 1; BEGIN BEGIN x := 9; END; \
             RAISE NOTICE 'after %', x; END; $$"
        ),
        vec!["after 9".to_string()]
    );
}

#[test]
fn an_inner_handler_catches_and_the_outer_block_carries_on() {
    let mut e = Engine::new();
    assert_eq!(
        notices(
            &mut e,
            "DO $$ BEGIN \
               BEGIN RAISE EXCEPTION 'boom'; \
               EXCEPTION WHEN others THEN RAISE NOTICE 'caught %', SQLERRM; END; \
               RAISE NOTICE 'continued'; END; $$"
        ),
        vec!["caught boom".to_string(), "continued".to_string()]
    );
}

#[test]
fn a_label_names_the_loop_or_block_a_jump_leaves() {
    let mut e = Engine::new();
    // EXIT <label> out of an inner loop leaves the OUTER one.
    assert_eq!(
        notices(
            &mut e,
            "DO $$ DECLARE i int; j int; BEGIN <<o>> FOR i IN 1..3 LOOP \
               FOR j IN 1..3 LOOP EXIT o WHEN i = 2; RAISE NOTICE '%,%', i, j; \
               END LOOP; END LOOP; END; $$"
        ),
        vec!["1,1".to_string(), "1,2".to_string(), "1,3".to_string()]
    );
    // CONTINUE <label> resumes the outer loop.
    assert_eq!(
        notices(
            &mut e,
            "DO $$ DECLARE i int; j int; BEGIN <<o>> FOR i IN 1..3 LOOP \
               FOR j IN 1..3 LOOP CONTINUE o WHEN j = 2; RAISE NOTICE '%,%', i, j; \
               END LOOP; END LOOP; END; $$"
        ),
        vec!["1,1".to_string(), "2,1".to_string(), "3,1".to_string()]
    );
    // And EXIT <label> naming a BLOCK leaves the block.
    assert_eq!(
        notices(
            &mut e,
            "DO $$ <<ob>> BEGIN BEGIN EXIT ob; END; RAISE NOTICE 'not reached'; END; $$"
        ),
        Vec::<String>::new()
    );
    // A labelled WHILE, and `END LOOP <label>`.
    assert_eq!(
        notices(
            &mut e,
            "DO $$ DECLARE i int := 0; BEGIN <<w>> WHILE i < 4 LOOP i := i + 1; \
             CONTINUE w WHEN i = 2; RAISE NOTICE 'i=%', i; END LOOP w; END; $$"
        ),
        vec!["i=1".to_string(), "i=3".to_string(), "i=4".to_string()]
    );
}

#[test]
fn an_end_label_must_agree_with_the_blocks() {
    let mut e = Engine::new();
    // PG 18.6's two sentences, word for word.
    assert!(
        err(&mut e, "DO $$ BEGIN NULL; END zz; $$")
            .contains("end label \"zz\" specified for unlabeled block"),
        "{}",
        err(&mut e, "DO $$ BEGIN NULL; END zz; $$")
    );
    assert!(
        err(&mut e, "DO $$ <<a>> BEGIN NULL; END b; $$")
            .contains("end label \"b\" differs from block's label \"a\""),
        "{}",
        err(&mut e, "DO $$ <<a>> BEGIN NULL; END b; $$")
    );
    // The agreeing form runs.
    assert_eq!(
        notices(&mut e, "DO $$ <<a>> BEGIN RAISE NOTICE 'ok'; END a; $$"),
        vec!["ok".to_string()]
    );
    // And so does the loop form of the same check.
    assert!(
        err(
            &mut e,
            "DO $$ DECLARE i int; BEGIN <<lp>> FOR i IN 1..2 LOOP NULL; END LOOP zz; END; $$"
        )
        .contains("end label \"zz\" differs from block's label \"lp\""),
        "{}",
        err(
            &mut e,
            "DO $$ DECLARE i int; BEGIN <<lp>> FOR i IN 1..2 LOOP NULL; END LOOP zz; END; $$"
        )
    );
}

#[test]
fn a_function_body_round_trips_through_its_stored_text() {
    let mut e = Engine::new();
    // CREATE FUNCTION stores the body by re-rendering the parsed block,
    // so a nested block that does not RENDER is a body that stops
    // working the moment it is stored.
    e.execute(
        "CREATE FUNCTION fb9(a int) RETURNS int AS $$
         <<top>>
         DECLARE r int := 0;
         BEGIN
           <<blk>>
           DECLARE r int := 100;
           BEGIN
             r := r + a;
           END blk;
           BEGIN
             r := a * 2;
           EXCEPTION WHEN others THEN
             r := -1;
           END;
           RETURN r;
         END top;
         $$ LANGUAGE plpgsql",
    )
    .unwrap();
    let QueryResult::Rows { rows, .. } = e.execute("SELECT fb9(5)").unwrap() else {
        panic!("expected rows");
    };
    assert_eq!(
        spg_engine::eval::value_to_text(&rows[0].values[0]),
        "10".to_string()
    );
}
