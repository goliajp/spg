//! v7.39 (read01 round 64) — a multi-statement plpgsql body is callable.
//!
//! The interpreter has existed since v7.12.4 — triggers and DO blocks run on it.
//! What was missing was the scalar entry: `resolve_return`'s own comment said
//! "the scalar UDF surface in a later release handles RETURN <expr> properly;
//! for now we fall through to Skip". This is that release. The body runs on the
//! SAME interpreter, with the arguments pre-bound as locals and RETURN actually
//! evaluated.
//!
//! Two bugs fell out of wiring it up, and both were older than this round:
//!
//!   - **`PlPgSqlBlock`'s Display did not render the EXCEPTION section**, and
//!     CREATE FUNCTION stores a body by re-rendering the parsed block through
//!     it. So every exception handler a function (or a trigger) declared was
//!     thrown away AT STORE TIME. A DO block never round-trips through text,
//!     which is why only the stored kinds lost theirs.
//!   - **`FOR rec IN SELECT … LOOP` bound only the first CELL** of each row, so
//!     `rec.v` answered "unknown table qualifier: rec". The loop now binds the
//!     whole row.
//!
//! Byte-locked against live PG18.4, with in-place MVCC on.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

fn r1(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE t (id int, v text)");
    ok(&mut e, "INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c')");
    e
}

#[test]
fn locals_and_assignment() {
    let mut e = seeded();
    ok(
        &mut e,
        "CREATE FUNCTION bump(x int) RETURNS int AS $$ BEGIN x := x + 1; RETURN x * 2; END; $$ LANGUAGE plpgsql",
    );
    assert_eq!(r1(&mut e, "SELECT bump(3)"), "8");
}

#[test]
fn declare_and_if_elsif_else() {
    let mut e = seeded();
    ok(
        &mut e,
        "CREATE FUNCTION grade(n int) RETURNS text AS $$ \
         DECLARE r text; \
         BEGIN \
           IF n >= 90 THEN r := 'A'; ELSIF n >= 80 THEN r := 'B'; ELSE r := 'C'; END IF; \
           RETURN r; \
         END; $$ LANGUAGE plpgsql",
    );
    assert_eq!(r1(&mut e, "SELECT grade(95)"), "A");
    assert_eq!(r1(&mut e, "SELECT grade(85)"), "B");
    assert_eq!(r1(&mut e, "SELECT grade(10)"), "C");
    // …and per-row, inside an aggregate.
    assert_eq!(
        r1(&mut e, "SELECT string_agg(grade(id*30), ',' ORDER BY id) FROM t"),
        "C,C,A"
    );
}

#[test]
fn select_into_sees_only_visible_rows() {
    let mut e = seeded();
    ok(
        &mut e,
        "CREATE FUNCTION total() RETURNS bigint AS $$ \
         DECLARE c bigint; BEGIN SELECT count(*) INTO c FROM t; RETURN c; END; $$ LANGUAGE plpgsql",
    );
    assert_eq!(r1(&mut e, "SELECT total()"), "3");
    // The body's queries go through the READ path, so a deleted row is gone for
    // them too — the same hazard round 63 flagged.
    ok(&mut e, "DELETE FROM t WHERE id = 2");
    assert_eq!(r1(&mut e, "SELECT total()"), "2");
}

#[test]
fn for_in_select_binds_the_whole_record() {
    // It used to bind only the first CELL, so `rec.v` said
    // "unknown table qualifier: rec".
    let mut e = seeded();
    ok(
        &mut e,
        "CREATE FUNCTION concat_all() RETURNS text AS $$ \
         DECLARE out text := ''; rec record; \
         BEGIN FOR rec IN SELECT v FROM t ORDER BY id LOOP out := out || rec.v; END LOOP; \
           RETURN out; END; $$ LANGUAGE plpgsql",
    );
    assert_eq!(r1(&mut e, "SELECT concat_all()"), "abc");
    ok(&mut e, "DELETE FROM t WHERE id = 2");
    assert_eq!(r1(&mut e, "SELECT concat_all()"), "ac");
}

#[test]
fn an_exception_handler_survives_being_stored() {
    // PlPgSqlBlock's Display did not render EXCEPTION, and CREATE FUNCTION
    // stores the body by re-rendering the parsed block — so the handler was
    // thrown away at STORE time and the RAISE escaped.
    let mut e = seeded();
    ok(
        &mut e,
        "CREATE FUNCTION guarded(x int) RETURNS text AS $$ \
         BEGIN IF x < 0 THEN RAISE EXCEPTION 'negative'; END IF; RETURN 'ok'; \
         EXCEPTION WHEN OTHERS THEN RETURN 'caught'; END; $$ LANGUAGE plpgsql",
    );
    assert_eq!(r1(&mut e, "SELECT guarded(1)"), "ok");
    assert_eq!(r1(&mut e, "SELECT guarded(-1)"), "caught");
}

#[test]
fn a_body_that_writes_is_refused_not_silently_dropped() {
    // The call arrives through expression evaluation, which holds the engine
    // immutably. Silently discarding the write would be the worst answer.
    let mut e = seeded();
    ok(
        &mut e,
        "CREATE FUNCTION writes(x int) RETURNS int AS $$ \
         BEGIN INSERT INTO t VALUES (x, 'w'); RETURN x; END; $$ LANGUAGE plpgsql",
    );
    let msg = err(&mut e, "SELECT writes(9)");
    assert!(msg.contains("cannot be called from an expression"), "{msg}");
    // And nothing was written.
    assert_eq!(r1(&mut e, "SELECT count(*) FROM t"), "3");
}

#[test]
fn falling_out_of_the_bottom_is_an_error() {
    let mut e = seeded();
    ok(
        &mut e,
        "CREATE FUNCTION no_return(x int) RETURNS int AS $$ BEGIN x := x + 1; END; $$ LANGUAGE plpgsql",
    );
    let msg = err(&mut e, "SELECT no_return(1)");
    assert!(msg.contains("without RETURN"), "{msg}");
}
