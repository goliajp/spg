//! v7.39 (round 652) — `ADD CONSTRAINT … CHECK` never looked at the rows
//! that were already there.
//!
//! The F31 audit went looking for tests and comments that assert SPG's own
//! gaps as rules. The parser carried this one:
//!
//! > SPG validates inline at ADD CONSTRAINT time (no NOT VALID / VALIDATE
//! > separation), so VALIDATE is an accept-and-no-op for pg_dump round-trip.
//!
//! Measured against PG18, the premise was false in both directions. The
//! `NOT VALID` suffix was a syntax error, so the constraint pg_dump emits
//! for a table PG deliberately left unvalidated could not be restored at
//! all. And plain `ADD CONSTRAINT` did not scan either — it installed the
//! constraint over rows that violate it, silently, so a table ended up
//! holding data contradicting its own declared CHECK while every reader
//! (pg_dump included) believed it did not. PG refuses that ALTER.
//!
//! The comment is why nobody looked: it named a correctness property in
//! the present tense, and the property did not exist.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
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
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(err) => format!("{err}"),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

/// PG: `check constraint "nv_pos" of relation "nv" is violated by some row`,
/// and the constraint is not installed — the later INSERT of another
/// negative row succeeds, leaving three.
#[test]
fn round652_a_plain_add_scans_the_rows_already_there() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE nv(a int)").unwrap();
    e.execute("INSERT INTO nv VALUES (1),(-5)").unwrap();

    let msg = err(&mut e, "ALTER TABLE nv ADD CONSTRAINT nv_pos CHECK (a > 0)");
    assert!(
        msg.contains("check constraint \"nv_pos\" of relation \"nv\" is violated by some row"),
        "{msg}"
    );

    // Refused means not installed: the table is untouched and still takes
    // negative rows. A half-applied ALTER would show up right here.
    e.execute("INSERT INTO nv VALUES (-9)").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM nv"), "3");
    assert_eq!(one(&mut e, "SELECT count(*) FROM pg_constraint WHERE conname='nv_pos'"), "0");
}

/// An unnamed CHECK gets the synthesised name in the message, the same
/// `<table>_<col>_check` form pg_constraint would give it.
#[test]
fn round652_b_the_message_names_the_constraint_it_would_have_installed() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE nv(a int)").unwrap();
    e.execute("INSERT INTO nv VALUES (-5)").unwrap();
    let msg = err(&mut e, "ALTER TABLE nv ADD CHECK (a > 0)");
    assert!(msg.contains("\"nv_a_check\""), "{msg}");
}

/// NOT VALID installs it without the scan. New rows are still checked —
/// that is the half people get wrong; NOT VALID grandfathers the past, it
/// does not disable the constraint.
#[test]
fn round652_c_not_valid_grandfathers_the_past_only() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE nv(a int)").unwrap();
    e.execute("INSERT INTO nv VALUES (1),(-5)").unwrap();
    e.execute("ALTER TABLE nv ADD CONSTRAINT nv_pos CHECK (a > 0) NOT VALID")
        .unwrap();

    let msg = err(&mut e, "INSERT INTO nv VALUES (-9)");
    assert!(msg.contains("violates check constraint"), "{msg}");
    assert_eq!(one(&mut e, "SELECT count(*) FROM nv"), "2");

    assert_eq!(
        one(
            &mut e,
            "SELECT convalidated FROM pg_constraint WHERE conname='nv_pos'"
        ),
        "false"
    );
    // The suffix pg_dump reads back. Without it the restore would validate
    // a constraint PG deliberately did not.
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname='nv_pos'"
        ),
        "CHECK ((a > 0)) NOT VALID"
    );
}

/// VALIDATE runs the scan that NOT VALID skipped: it refuses while the
/// offending rows are there and succeeds once they are gone, flipping both
/// `convalidated` and the deparse.
#[test]
fn round652_d_validate_runs_the_deferred_scan() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE nv(a int)").unwrap();
    e.execute("INSERT INTO nv VALUES (1),(-5)").unwrap();
    e.execute("ALTER TABLE nv ADD CONSTRAINT nv_pos CHECK (a > 0) NOT VALID")
        .unwrap();

    let msg = err(&mut e, "ALTER TABLE nv VALIDATE CONSTRAINT nv_pos");
    assert!(msg.contains("is violated by some row"), "{msg}");
    assert_eq!(
        one(
            &mut e,
            "SELECT convalidated FROM pg_constraint WHERE conname='nv_pos'"
        ),
        "false"
    );

    // The DELETE tombstones the row rather than removing it; the scan has
    // to skip tombstones or this second VALIDATE would refuse too.
    e.execute("DELETE FROM nv WHERE a < 0").unwrap();
    e.execute("ALTER TABLE nv VALIDATE CONSTRAINT nv_pos").unwrap();
    assert_eq!(
        one(
            &mut e,
            "SELECT convalidated FROM pg_constraint WHERE conname='nv_pos'"
        ),
        "true"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname='nv_pos'"
        ),
        "CHECK ((a > 0))"
    );
}

/// Validating an already-valid constraint is a no-op; naming one that does
/// not exist is PG's 42704.
#[test]
fn round652_e_validate_edges() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE nv(a int)").unwrap();
    e.execute("ALTER TABLE nv ADD CONSTRAINT nv_pos CHECK (a > 0)")
        .unwrap();
    e.execute("ALTER TABLE nv VALIDATE CONSTRAINT nv_pos").unwrap();

    let msg = err(&mut e, "ALTER TABLE nv VALIDATE CONSTRAINT nope");
    assert!(
        msg.contains("constraint \"nope\" of relation \"nv\" does not exist"),
        "{msg}"
    );
}

/// A CHECK written inside CREATE TABLE cannot be NOT VALID — there are no
/// rows to grandfather, and PG rejects the suffix there.
#[test]
fn round652_f_not_valid_is_an_alter_only_suffix() {
    let mut e = Engine::new();
    assert!(e.execute("CREATE TABLE nv(a int CHECK (a > 0) NOT VALID)").is_err());
    assert!(
        e.execute("CREATE TABLE nv2(a int, CONSTRAINT c CHECK (a > 0) NOT VALID)")
            .is_err()
    );
}

/// `oid = 'pg_class'::regclass` worked and `oid IN ('pg_class'::regclass, …)`
/// did not: the IN list ran a static type check before comparing anything,
/// and a reg cast describes as Text because SPG has no reg `DataType`. The
/// ANY form failed one layer further down — the array constructor classified
/// reg values by the catch-all arm and built a text array.
#[test]
fn round652_g_reg_casts_survive_in_lists_and_arrays() {
    let mut e = Engine::new();
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_class WHERE oid = 'pg_class'::regclass"
        ),
        "1"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_class WHERE oid IN ('pg_class'::regclass)"
        ),
        "1"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_class \
             WHERE oid IN ('pg_class'::regclass, 'pg_type'::regclass)"
        ),
        "2"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_class WHERE oid = ANY(ARRAY['pg_class'::regclass])"
        ),
        "1"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_type WHERE oid IN ('int4'::regtype, 'text'::regtype)"
        ),
        "2"
    );
}

/// The array built from reg values is a bigint array, not text: the
/// classification arm and the materialisation arm are two lists that have
/// to agree, and teaching only the first one panicked on the wire.
#[test]
fn round652_h_a_reg_array_is_an_oid_array() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(ARRAY['pg_class'::regclass])"),
        "bigint[]"
    );
    assert_eq!(
        one(&mut e, "SELECT (ARRAY['int4'::regtype])[1] = 'int4'::regtype::oid"),
        "true"
    );
}

/// v7.39 (round 652) — dropping a constraint must not move another one's
/// NOT VALID mark onto it. The mark rides on the constraint itself now, so
/// this is structurally safe — but it was an index-parallel vector for a
/// while (an attempt to dodge a perf effect that four broken instruments
/// had overstated), and under that shape a mis-shifted index would have
/// been invisible: nothing goes red, pg_dump just emits NOT VALID for the
/// wrong constraint. The pin stays as the regression guard for whatever
/// shape the mark takes next.
#[test]
fn round652_i_dropping_a_constraint_leaves_the_other_marks_alone() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE nv(a int, b int)").unwrap();
    e.execute("INSERT INTO nv VALUES (1, -5)").unwrap();
    e.execute("ALTER TABLE nv ADD CONSTRAINT c_a CHECK (a > 0)")
        .unwrap();
    e.execute("ALTER TABLE nv ADD CONSTRAINT c_b CHECK (b > 0) NOT VALID")
        .unwrap();
    assert_eq!(
        one(
            &mut e,
            "SELECT conname, convalidated FROM pg_constraint \
             WHERE contype='c' ORDER BY conname"
        ),
        "c_a|true,c_b|false"
    );

    // c_a occupies the slot before c_b; dropping it shifts c_b down one.
    e.execute("ALTER TABLE nv DROP CONSTRAINT c_a").unwrap();
    assert_eq!(
        one(
            &mut e,
            "SELECT conname, convalidated FROM pg_constraint \
             WHERE contype='c' ORDER BY conname"
        ),
        "c_b|false",
        "the mark has to stay on c_b"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname='c_b'"
        ),
        "CHECK ((b > 0)) NOT VALID"
    );

    e.execute("DELETE FROM nv WHERE b < 0").unwrap();
    e.execute("ALTER TABLE nv VALIDATE CONSTRAINT c_b").unwrap();
    assert_eq!(
        one(
            &mut e,
            "SELECT convalidated FROM pg_constraint WHERE conname='c_b'"
        ),
        "true"
    );
}
