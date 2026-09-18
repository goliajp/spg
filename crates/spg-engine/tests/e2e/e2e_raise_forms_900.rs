//! 9.0.0 — PL/pgSQL's whole `RAISE` grammar.
//!
//! SPG read a level word unconditionally and then demanded a format
//! string, so every spelling but `RAISE <level> '…'` was a syntax
//! error — and the error a block did raise always reached the client as
//! `P0001`, whatever the block asked for.
//!
//! ```text
//!                                              PG 18.6                SPG 8.0.4
//!   RAISE EXCEPTION SQLSTATE '22012'           22012: 22012           syntax error at end of input
//!   RAISE SQLSTATE '22012'                     22012: 22012           syntax error at end of input
//!   RAISE division_by_zero                     22012: division_by…    syntax error at end of input
//!   RAISE EXCEPTION 'b' USING ERRCODE='22012'  22012: b               syntax error at end of input
//!   RAISE EXCEPTION 'd' USING DETAIL='dd'      d + DETAIL: dd         syntax error at end of input
//! ```
//!
//! Every expectation below is PostgreSQL 18.6's, SQLSTATE included
//! (read with `\set VERBOSITY verbose`).

use spg_engine::Engine;

/// The `(SQLSTATE, message)` a DO block's failure reaches a client as.
fn raised(e: &mut Engine, body: &str) -> (String, String) {
    let sql = format!("DO $$ BEGIN {body} END $$");
    let err = e.execute(&sql).expect_err(&sql);
    let (code, msg) = spg_engine::sqlstate::error_to_wire(&err);
    (code.into_owned(), msg)
}

#[test]
fn a_raise_can_name_its_sqlstate() {
    let mut e = Engine::new();
    for (body, code, msg) in [
        ("RAISE EXCEPTION SQLSTATE '22012';", "22012", "22012"),
        ("RAISE SQLSTATE '22012';", "22012", "22012"),
        (
            "RAISE EXCEPTION 'boom' USING ERRCODE = '22012';",
            "22012",
            "boom",
        ),
        (
            "RAISE EXCEPTION 'boom' USING ERRCODE = 'ZZ999';",
            "ZZ999",
            "boom",
        ),
        // Not five characters: a condition NAME, resolved.
        (
            "RAISE EXCEPTION 'boom' USING ERRCODE = 'division_by_zero';",
            "22012",
            "boom",
        ),
        // None named: PG's default for EXCEPTION.
        ("RAISE EXCEPTION 'plain';", "P0001", "plain"),
    ] {
        assert_eq!(
            raised(&mut e, body),
            (code.to_string(), msg.to_string()),
            "{body}"
        );
    }
}

/// A condition name on its own is both the code and the message.
#[test]
fn a_condition_name_raises_its_own_code() {
    let mut e = Engine::new();
    assert_eq!(
        raised(&mut e, "RAISE division_by_zero;"),
        ("22012".to_string(), "division_by_zero".to_string())
    );
    assert_eq!(
        raised(&mut e, "RAISE EXCEPTION division_by_zero;"),
        ("22012".to_string(), "division_by_zero".to_string())
    );
}

#[test]
fn a_name_that_is_no_condition_is_refused() {
    let mut e = Engine::new();
    assert_eq!(
        raised(&mut e, "RAISE nosuchcondition;").0,
        "42704".to_string()
    );
    assert!(
        raised(&mut e, "RAISE nosuchcondition;")
            .1
            .contains("unrecognized exception condition \"nosuchcondition\"")
    );
    assert_eq!(
        raised(&mut e, "RAISE EXCEPTION 'boom' USING ERRCODE = 'nosuch';").0,
        "42704".to_string()
    );
}

/// `SQLSTATE 'x'` checks the shape where it is written, as PG does.
#[test]
fn a_sqlstate_literal_must_be_a_code() {
    let mut e = Engine::new();
    for bad in ["'division_by_zero'", "'abc'"] {
        let (code, msg) = raised(&mut e, &format!("RAISE SQLSTATE {bad};"));
        assert_eq!(code, "42601", "{bad}");
        assert!(
            msg.contains(&format!("invalid SQLSTATE code at or near \"{bad}\"")),
            "{bad}: {msg}"
        );
    }
}

#[test]
fn using_detail_and_hint_reach_the_client() {
    let mut e = Engine::new();
    let (_, msg) = raised(
        &mut e,
        "RAISE EXCEPTION 'd' USING DETAIL = 'dd', HINT = 'hh';",
    );
    let (main, detail, hint) = spg_engine::sqlstate::split_detail_and_hint(&msg);
    assert_eq!(main, "d");
    assert_eq!(detail, Some("dd"));
    assert_eq!(hint, Some("hh"));
}

/// The spelling that always worked still does, arguments and all.
#[test]
fn the_format_string_form_is_unchanged() {
    let mut e = Engine::new();
    assert_eq!(
        raised(&mut e, "RAISE EXCEPTION 'plain %', 1;"),
        ("P0001".to_string(), "plain 1".to_string())
    );
    // A NOTICE is not an error, and `NULL;` is a statement.
    e.execute("DO $$ BEGIN RAISE NOTICE 'hi %', 2; END $$")
        .expect("NOTICE");
    e.execute("DO $$ BEGIN NULL; END $$").expect("NULL");
}
