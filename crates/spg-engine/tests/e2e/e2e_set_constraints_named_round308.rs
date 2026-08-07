//! v7.39 (round 308, V29) — `SET CONSTRAINTS <name>` means that name.
//!
//! Round 288 implemented the ALL form (what pg_dump emits, and what a
//! circular-FK restore needs) and recorded the named form as behaving
//! like ALL. That is a silent WRONG ACCEPT, not just a missing feature:
//! `SET CONSTRAINTS fk_a DEFERRED` also deferred fk_b, so a violation on
//! another table sailed past the statement that caused it and only
//! surfaced — if at all — at COMMIT.
//!
//! A prerequisite turned up while measuring: a name written on an INLINE
//! reference (`pid int CONSTRAINT fk_a REFERENCES p(id)`) was dropped by
//! the column-definition parser and replaced with the synthesised
//! `c_pid_fkey`. PG reports the declared name in violation messages, and
//! `SET CONSTRAINTS fk_a` has to match it, so that spelling had to keep
//! its name before any of this could work.
//!
//! All expectations read off live PG 18.4 (2026-07-21).

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"));
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(v) => panic!("{sql}: expected an error, got {v:?}"),
    }
}

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    for s in [
        "CREATE TABLE p29 (id int PRIMARY KEY)",
        "INSERT INTO p29 VALUES (1)",
        "CREATE TABLE c29a (id int, pid int CONSTRAINT fk_a REFERENCES p29(id) DEFERRABLE)",
        "CREATE TABLE c29b (id int, pid int CONSTRAINT fk_b REFERENCES p29(id) DEFERRABLE)",
        "CREATE TABLE c29c (id int, pid int CONSTRAINT fk_c REFERENCES p29(id))",
    ] {
        ok(&mut e, s);
    }
    e
}

/// The prerequisite: an inline `CONSTRAINT <name> REFERENCES` keeps its
/// name. Without this the named form has nothing to match on, and a
/// violation quotes a name the user never wrote.
#[test]
fn an_inline_reference_keeps_its_declared_name() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE p (id int PRIMARY KEY)");
    ok(
        &mut e,
        "CREATE TABLE c1 (id int, pid int CONSTRAINT fk_col REFERENCES p(id))",
    );
    ok(
        &mut e,
        "CREATE TABLE c2 (id int, pid int, CONSTRAINT fk_tbl FOREIGN KEY (pid) REFERENCES p(id))",
    );
    // An unnamed one still gets PG's synthesised name.
    ok(&mut e, "CREATE TABLE c3 (id int, pid int REFERENCES p(id))");
    let names = match e
        .execute("SELECT conname FROM pg_constraint WHERE contype = 'f' ORDER BY conname")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert_eq!(names, ["c3_pid_fkey", "fk_col", "fk_tbl"]);
    // And the declared name is what a violation quotes.
    assert!(
        err(&mut e, "INSERT INTO c1 VALUES (1, 999)").contains("foreign key constraint \"fk_col\"")
    );
}

/// The headline: naming one constraint must not defer the others.
#[test]
fn naming_one_constraint_defers_only_that_one() {
    let mut e = fixture();
    ok(&mut e, "BEGIN");
    ok(&mut e, "SET CONSTRAINTS fk_a DEFERRED");
    // fk_a is deferred, so its violation waits.
    ok(&mut e, "INSERT INTO c29a VALUES (1, 999)");
    // fk_b was never named, so its violation is immediate — and quotes
    // its own name.
    assert!(
        err(&mut e, "INSERT INTO c29b VALUES (1, 999)").contains("foreign key constraint \"fk_b\"")
    );
    ok(&mut e, "ROLLBACK");
}

#[test]
fn naming_several_defers_each_of_them() {
    let mut e = fixture();
    ok(&mut e, "BEGIN");
    ok(&mut e, "SET CONSTRAINTS fk_a, fk_b DEFERRED");
    ok(&mut e, "INSERT INTO c29a VALUES (1, 999)");
    ok(&mut e, "INSERT INTO c29b VALUES (1, 999)");
    // Both waited; COMMIT is where they are paid for.
    assert!(err(&mut e, "COMMIT").contains("foreign key constraint"));
}

#[test]
fn a_bad_name_is_refused_before_anything_changes() {
    let mut e = fixture();
    ok(&mut e, "BEGIN");
    assert!(
        err(&mut e, "SET CONSTRAINTS nosuch_fk DEFERRED")
            .contains("constraint \"nosuch_fk\" does not exist")
    );
    ok(&mut e, "ROLLBACK");

    // Naming a constraint that cannot be deferred is its own error.
    ok(&mut e, "BEGIN");
    assert!(
        err(&mut e, "SET CONSTRAINTS fk_c DEFERRED")
            .contains("constraint \"fk_c\" is not deferrable")
    );
    ok(&mut e, "ROLLBACK");
}

/// A named IMMEDIATE settles the constraints it names and leaves the
/// rest queued. Draining everything would report a violation the
/// statement never asked about.
#[test]
fn a_named_immediate_settles_only_what_it_names() {
    let mut e = fixture();
    ok(&mut e, "BEGIN");
    ok(&mut e, "SET CONSTRAINTS ALL DEFERRED");
    // fk_a's row is fine; fk_b's is not.
    ok(&mut e, "INSERT INTO c29a VALUES (1, 1)");
    ok(&mut e, "INSERT INTO c29b VALUES (1, 999)");
    // Settling fk_a alone must succeed — fk_b's bad row is not its
    // business, and stays queued.
    ok(&mut e, "SET CONSTRAINTS fk_a IMMEDIATE");
    assert!(err(&mut e, "COMMIT").contains("foreign key constraint \"fk_b\""));
}

/// A later blanket setting replaces the per-name ones, and a per-name
/// setting overrides an earlier blanket one. Both directions, because
/// getting only one right still leaves a silent wrong accept.
#[test]
fn blanket_and_named_settings_compose_the_way_pg_orders_them() {
    let mut e = fixture();
    // named IMMEDIATE wins over an earlier ALL DEFERRED
    ok(&mut e, "BEGIN");
    ok(&mut e, "SET CONSTRAINTS ALL DEFERRED");
    ok(&mut e, "SET CONSTRAINTS fk_a IMMEDIATE");
    assert!(
        err(&mut e, "INSERT INTO c29a VALUES (1, 999)").contains("foreign key constraint \"fk_a\"")
    );
    ok(&mut e, "ROLLBACK");

    // ALL DEFERRED afterwards clears the per-name override again
    ok(&mut e, "BEGIN");
    ok(&mut e, "SET CONSTRAINTS fk_a IMMEDIATE");
    ok(&mut e, "SET CONSTRAINTS ALL DEFERRED");
    ok(&mut e, "INSERT INTO c29a VALUES (1, 999)");
    assert!(err(&mut e, "COMMIT").contains("foreign key constraint \"fk_a\""));
}

/// The ALL form that round 288 built — and that pg_dump actually emits —
/// keeps working unchanged.
#[test]
fn the_all_form_is_unchanged() {
    let mut e = fixture();
    ok(&mut e, "BEGIN");
    ok(&mut e, "SET CONSTRAINTS ALL DEFERRED");
    ok(&mut e, "INSERT INTO c29a VALUES (1, 999)");
    ok(&mut e, "INSERT INTO c29b VALUES (1, 999)");
    assert!(err(&mut e, "COMMIT").contains("foreign key constraint"));

    // ALL IMMEDIATE drains at the statement, not at COMMIT.
    ok(&mut e, "BEGIN");
    ok(&mut e, "SET CONSTRAINTS ALL DEFERRED");
    ok(&mut e, "INSERT INTO c29a VALUES (1, 999)");
    assert!(err(&mut e, "SET CONSTRAINTS ALL IMMEDIATE").contains("foreign key constraint"));
    ok(&mut e, "ROLLBACK");
}

/// Round-trip: the statement renders back with its names, so a
/// bind-final render or a dump keeps meaning the same thing.
#[test]
fn the_named_form_round_trips_through_display() {
    let mut e = fixture();
    assert_eq!(
        one(&mut e, "SELECT 1"),
        "1",
        "sanity: fixture engine answers"
    );
    let rendered = spg_sql::parser::parse_statement("SET CONSTRAINTS fk_a, fk_b DEFERRED")
        .unwrap()
        .to_string();
    assert_eq!(rendered, "SET CONSTRAINTS fk_a, fk_b DEFERRED");
    let all = spg_sql::parser::parse_statement("SET CONSTRAINTS ALL IMMEDIATE")
        .unwrap()
        .to_string();
    assert_eq!(all, "SET CONSTRAINTS ALL IMMEDIATE");
}
