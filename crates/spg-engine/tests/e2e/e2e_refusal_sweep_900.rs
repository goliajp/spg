//! 9.0.0 (S1) — the refusal sweep: every `unsupported` sentence in the
//! engine, measured against PostgreSQL 18.6 to see which of them refuse
//! something PostgreSQL does.
//!
//! Six did, and each is closed here. The wording of two more was SPG's
//! own where PostgreSQL has its own sentence.
//!
//! ```text
//!                                               PG 18.6      SPG 8.0.4
//!   CREATE INDEX … USING brin ((a+1))           ok           refused
//!   CREATE INDEX … USING brin (lower(b))        ok           refused
//!   SELECT g FROM t ORDER BY g   (a range)      ok           refused
//!   … RANGE BETWEEN '1 day' PRECEDING …         ok           syntax error
//!   ALTER FUNCTION f() RENAME TO g              ok           refused
//!   SELECT count(*) OVER ()      (no FROM)      1            refused
//!   CREATE INDEX … USING gin (f) INCLUDE (a)    its sentence SPG's own
//!   CREATE RULE … ON SELECT                     its sentence SPG's own
//! ```
//!
//! **Not closed, and why:** `UPDATE t SET e[-1] = 9` is accepted by
//! PostgreSQL, which extends the array leftwards and gives it a new
//! LOWER BOUND. SPG's arrays have no lower bound to give — N29, an
//! owner decision already taken for 9.0.0 — so the refusal stands until
//! that representation does.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
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
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn run(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    run(
        &mut e,
        "CREATE TABLE s1t(a int, b text, c numeric, d date, e int[], f jsonb, g int4range)",
    );
    run(
        &mut e,
        "INSERT INTO s1t VALUES (1,'x',1,'2020-01-01','{1,2}','{\"k\":1}','[1,5)')",
    );
    e
}

/// PostgreSQL accepts an expression key on BRIN; SPG refused it, so a
/// schema that had one did not load.
#[test]
fn s1_brin_takes_an_expression_key() {
    let mut e = fixture();
    run(&mut e, "CREATE INDEX s1_brin ON s1t USING brin ((a+1))");
    run(&mut e, "CREATE INDEX s1_brin2 ON s1t USING brin (lower(b))");
    // A7's contract holds: the catalog names the access method the
    // statement asked for, whatever backs it.
    assert_eq!(
        vals(
            &mut e,
            "SELECT indexdef FROM pg_indexes WHERE indexname = 's1_brin' "
        )
        .len(),
        1
    );
    let def = vals(
        &mut e,
        "SELECT indexdef FROM pg_indexes WHERE indexname = 's1_brin'",
    );
    assert!(def[0].contains("USING brin"), "{def:?}");
}

/// A range orders the way PostgreSQL orders one.
#[test]
fn s1_a_range_is_orderable() {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE s1r(g int4range)");
    for v in ["[1,5)", "[1,5]", "(1,5)", "[1,)", "(,5)", "empty", "[2,3)"] {
        run(&mut e, &format!("INSERT INTO s1r VALUES ('{v}')"));
    }
    // PostgreSQL 18.6's own order, measured — it canonicalises the
    // discrete bounds first, which SPG does too.
    assert_eq!(
        vals(&mut e, "SELECT g FROM s1r ORDER BY g"),
        vec!["empty", "(,5)", "[1,5)", "[1,6)", "[1,)", "[2,3)", "[2,5)"]
    );
}

/// A window function with no FROM runs over the one virtual row.
#[test]
fn s1_a_window_function_needs_no_from() {
    let mut e = Engine::new();
    assert_eq!(vals(&mut e, "SELECT count(*) OVER ()"), vec!["1"]);
    assert_eq!(vals(&mut e, "SELECT row_number() OVER ()"), vec!["1"]);
    assert_eq!(vals(&mut e, "SELECT sum(2) OVER ()"), vec!["2"]);
}

/// `RANGE BETWEEN '1 day' PRECEDING` — a bare string offset, which
/// PostgreSQL coerces to the ORDER BY column's type.
#[test]
fn s1_a_bare_string_frame_offset_parses() {
    let mut e = fixture();
    assert_eq!(
        vals(
            &mut e,
            "SELECT sum(a) OVER (ORDER BY d RANGE BETWEEN '1 day' PRECEDING \
             AND '1 day' FOLLOWING) FROM s1t"
        ),
        vec!["1"]
    );
}

/// `ALTER FUNCTION f() RENAME TO g` moves the one overload, and refuses
/// an ambiguous name the way `DROP FUNCTION` does.
#[test]
fn s1_alter_function_rename_moves_it() {
    let mut e = Engine::new();
    run(
        &mut e,
        "CREATE FUNCTION s1f() RETURNS int LANGUAGE sql AS 'SELECT 1'",
    );
    run(&mut e, "ALTER FUNCTION s1f() RENAME TO s1f2");
    assert_eq!(vals(&mut e, "SELECT s1f2()"), vec!["1"]);
    assert!(e.execute("SELECT s1f()").is_err(), "the old name is gone");
    // Two overloads, and the statement cannot say which.
    run(
        &mut e,
        "CREATE FUNCTION s1h(int) RETURNS int LANGUAGE sql AS 'SELECT 1'",
    );
    run(
        &mut e,
        "CREATE FUNCTION s1h(text) RETURNS int LANGUAGE sql AS 'SELECT 2'",
    );
    let err = e
        .execute("ALTER FUNCTION s1h(int) RENAME TO s1h2")
        .expect_err("ambiguous");
    assert!(format!("{err}").contains("is not unique"), "{err}");
}

/// 9.0.0 — COPY may fill a `GENERATED ALWAYS AS IDENTITY` column and
/// INSERT may not.
///
/// Measured on PostgreSQL 18.6: `COPY t (id, n) FROM stdin` into such a
/// column takes the value, and the identical `INSERT` answers `cannot
/// insert a non-DEFAULT value into column "id"`. `pg_dump` relies on
/// that — it writes a table's data as COPY — and COPY rides the INSERT
/// path here, so it inherited a refusal PostgreSQL does not make and a
/// dump of an identity table did not restore. Caught by the dump-compat
/// fixture panel, which is the only instrument that reads a whole dump
/// back.
#[test]
fn s1_copy_may_fill_a_generated_always_identity_column() {
    let sql = spg_engine::copy::build_copy_insert(
        "t",
        Some(&[alloc_string("id"), alloc_string("n")]),
        &[Some(alloc_string("9")), Some(alloc_string("x"))],
    );
    assert!(
        sql.contains("OVERRIDING SYSTEM VALUE"),
        "COPY's insert carries PostgreSQL's own spelling for the          permission: {sql}"
    );
    // …and it round-trips through the engine.
    let mut e = Engine::new();
    run(
        &mut e,
        "CREATE TABLE ident9(id int GENERATED ALWAYS AS IDENTITY, n text)",
    );
    run(
        &mut e,
        &sql.replace("INSERT INTO t ", "INSERT INTO ident9 "),
    );
    assert_eq!(vals(&mut e, "SELECT id, n FROM ident9"), vec!["9|x"]);
    // The plain INSERT is still refused, as PostgreSQL refuses it.
    let err = e
        .execute("INSERT INTO ident9(id, n) VALUES (8, 'y')")
        .expect_err("PG refuses it");
    assert!(
        format!("{err}").contains("cannot insert a non-DEFAULT value into column \"id\""),
        "{err}"
    );
}

fn alloc_string(s: &str) -> String {
    String::from(s)
}

/// Two refusals PostgreSQL also makes, in PostgreSQL's words.
#[test]
fn s1_the_shared_refusals_use_postgresqls_sentence() {
    let mut e = fixture();
    let err = e
        .execute("CREATE INDEX s1_gin ON s1t USING gin (f) INCLUDE (a)")
        .expect_err("both refuse it");
    assert_eq!(
        format!("{err}"),
        "unsupported: access method \"gin\" does not support included columns"
    );
    let err = e
        .execute("CREATE RULE s1r AS ON SELECT TO s1t DO INSTEAD SELECT 1")
        .expect_err("both refuse it");
    assert!(
        format!("{err}").contains("relation \"s1t\" cannot have ON SELECT rules"),
        "{err}"
    );
}
