//! v7.39 (round 312, V33) — `pg_get_functiondef` / `pg_get_ruledef`.
//!
//! Round 290 planned both alongside the constraint deparse and covered
//! only constraints; these two stayed stubs answering NULL, and
//! `pg_rewrite` — the catalogue `pg_get_ruledef(oid)` resolves against —
//! did not exist at all, so there was no way to reach a rule by oid.
//!
//! PG returns a complete, re-runnable statement from both, which is what
//! reflection tooling and pg_dump read. The layouts are load-bearing and
//! were measured byte-for-byte against 18.4:
//!
//!   * functiondef continues each clause on its own line with ONE leading
//!     space, delimits the body with `$function$`, and ends with a
//!     newline;
//!   * ruledef breaks after `AS`, indents the event line by four, and
//!     puts either `INSTEAD ` or a second space after `DO ` — so a DO
//!     ALSO rule reads `DO  INSERT …` with the gap where INSTEAD would
//!     have gone.
//!
//! `pg_rules.definition` is the same text: PG shows it schema-qualified,
//! i.e. the DEFAULT form, not the pretty one. It used to be a
//! single-line reconstruction of its own; both now come off one renderer
//! so they cannot drift.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    for s in [
        "CREATE TABLE r33 (id int, v int)",
        "CREATE FUNCTION f33_sql(a int, b int) RETURNS int LANGUAGE sql AS $$ SELECT a + b $$",
        "CREATE FUNCTION f33_noargs() RETURNS text LANGUAGE sql AS $$ SELECT 'hi'::text $$",
        "CREATE RULE r33_nothing AS ON DELETE TO r33 DO INSTEAD NOTHING",
        "CREATE RULE r33_also AS ON INSERT TO r33 DO ALSO INSERT INTO r33 VALUES (99, 99)",
    ] {
        e.execute(s).unwrap_or_else(|x| panic!("{s}: {x:?}"));
    }
    e
}

/// Byte-for-byte against PG 18.4, trailing newline included. The type
/// words are canonicalised — the function was declared `int`, PG prints
/// `integer` whatever the declaration said.
#[test]
fn functiondef_matches_pg_byte_for_byte() {
    let mut e = fixture();
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_get_functiondef(oid) FROM pg_proc WHERE proname='f33_sql'"
        ),
        "CREATE OR REPLACE FUNCTION public.f33_sql(a integer, b integer)\n \
         RETURNS integer\n LANGUAGE sql\nAS $function$ SELECT a + b $function$\n"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_get_functiondef(oid) FROM pg_proc WHERE proname='f33_noargs'"
        ),
        "CREATE OR REPLACE FUNCTION public.f33_noargs()\n \
         RETURNS text\n LANGUAGE sql\nAS $function$ SELECT 'hi'::text $function$\n"
    );
    // An oid nothing owns is NULL, not an error.
    assert_eq!(one(&mut e, "SELECT pg_get_functiondef(999999)"), "NULL");
}

/// `DO INSTEAD NOTHING` has no action body to deparse, so this one is
/// exact in both spellings — the layout is the whole content.
#[test]
fn ruledef_matches_pg_for_a_rule_with_no_action() {
    let mut e = fixture();
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_get_ruledef(oid) FROM pg_rewrite WHERE rulename='r33_nothing'"
        ),
        "CREATE RULE r33_nothing AS\n    ON DELETE TO public.r33 DO INSTEAD NOTHING;"
    );
    // The pretty spelling drops the schema qualification, and nothing else.
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_get_ruledef(oid, true) FROM pg_rewrite WHERE rulename='r33_nothing'"
        ),
        "CREATE RULE r33_nothing AS\n    ON DELETE TO r33 DO INSTEAD NOTHING;"
    );
}

/// A DO ALSO rule: the frame is PG's, including the double space where
/// INSTEAD would have gone. The action body is the text as stored — PG
/// re-deparses it from the parse tree and so writes a column list, which
/// is a recorded difference (V47), not something this layout controls.
#[test]
fn ruledef_frames_a_do_also_rule_the_way_pg_does() {
    let mut e = fixture();
    let def = one(
        &mut e,
        "SELECT pg_get_ruledef(oid) FROM pg_rewrite WHERE rulename='r33_also'",
    );
    assert!(
        def.starts_with("CREATE RULE r33_also AS\n    ON INSERT TO public.r33 DO  INSERT"),
        "got {def:?}"
    );
    assert!(def.ends_with(';'), "got {def:?}");
    let pretty = one(
        &mut e,
        "SELECT pg_get_ruledef(oid, true) FROM pg_rewrite WHERE rulename='r33_also'",
    );
    assert!(
        pretty.starts_with("CREATE RULE r33_also AS\n    ON INSERT TO r33 DO  INSERT"),
        "got {pretty:?}"
    );
}

/// The catalogue itself. Without it there is no oid to pass, so the
/// function could not be reached the way PG's own queries reach it.
#[test]
fn pg_rewrite_lists_the_rules() {
    let mut e = fixture();
    // PG's ev_type is a single char: 3 INSERT, 4 DELETE.
    assert_eq!(
        one(
            &mut e,
            "SELECT ev_type FROM pg_rewrite WHERE rulename='r33_also'"
        ),
        "3"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT ev_type FROM pg_rewrite WHERE rulename='r33_nothing'"
        ),
        "4"
    );
    // NB the engine's own renderer spells a bool `true`; `t` is psql's
    // dialect, and copying it from the oracle is a recurring trap.
    assert_eq!(
        one(
            &mut e,
            "SELECT is_instead FROM pg_rewrite WHERE rulename='r33_nothing'"
        ),
        "true"
    );
    // ev_class has to be the oid pg_class hands out for that table, or a
    // join against the other catalogues quietly returns nothing.
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_rewrite r JOIN pg_class c ON c.oid = r.ev_class \
             WHERE c.relname = 'r33'"
        ),
        "2"
    );
}

/// `pg_rules.definition` IS the default deparse, so the two must agree —
/// they now come off one renderer.
#[test]
fn pg_rules_definition_is_the_default_deparse() {
    let mut e = fixture();
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_rules v JOIN pg_rewrite r ON r.rulename = v.rulename \
             WHERE v.definition = pg_get_ruledef(r.oid)"
        ),
        "2"
    );
    // And it is the multi-line form, not the old single-line one.
    assert!(
        one(
            &mut e,
            "SELECT definition FROM pg_rules WHERE rulename='r33_nothing'"
        )
        .contains("AS\n    ON DELETE")
    );
}
