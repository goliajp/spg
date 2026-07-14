//! v7.39 (read01 utils/adt→commands/, round 45) — DDL object-error
//! wording aligned with PG18 and a real correctness fix: a table may
//! have at most one PRIMARY KEY. Error text byte-locked; the wire maps
//! these to 42P16 / 42701 / 42703 / 42P01 / 428C9 (see the pgwire
//! engine_error_sqlstate_tests).

use spg_engine::Engine;

fn err(e: &mut Engine, sql: &str) -> String {
    // Display (not Debug) so quotes aren't backslash-escaped; the
    // "unsupported: " prefix is stripped at the wire for typed states.
    format!("{}", e.execute(sql).unwrap_err())
}

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn second_primary_key_is_rejected() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE t3(a int PRIMARY KEY)");
    // A second PK — even on the same column — is an error (PG 42P16).
    assert!(
        err(&mut e, "ALTER TABLE t3 ADD PRIMARY KEY (a)")
            .contains("multiple primary keys for table \"t3\" are not allowed")
    );
    ok(&mut e, "CREATE TABLE t4(a int)");
    ok(&mut e, "ALTER TABLE t4 ADD PRIMARY KEY (a)");
    assert!(
        err(&mut e, "ALTER TABLE t4 ADD PRIMARY KEY (a)")
            .contains("multiple primary keys for table \"t4\" are not allowed")
    );
}

#[test]
fn ddl_object_error_wording_matches_pg() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE t1(a int, b text)");
    // duplicate column on ADD COLUMN.
    assert!(
        err(&mut e, "ALTER TABLE t1 ADD COLUMN a int")
            .contains("column \"a\" of relation \"t1\" already exists")
    );
    // missing column on DROP COLUMN.
    assert!(
        err(&mut e, "ALTER TABLE t1 DROP COLUMN nope")
            .contains("column \"nope\" of relation \"t1\" does not exist")
    );
    // missing table on DROP TABLE.
    assert!(
        err(&mut e, "DROP TABLE nonexist_tbl")
            .contains("table \"nonexist_tbl\" does not exist")
    );
}

#[test]
fn identity_generated_always_error_has_detail_hint() {
    let mut e = Engine::new();
    ok(
        &mut e,
        "CREATE TABLE idt(a int GENERATED ALWAYS AS IDENTITY, b text)",
    );
    ok(&mut e, "INSERT INTO idt(b) VALUES ('x')");
    let msg = err(&mut e, "INSERT INTO idt(a,b) VALUES (100,'z')");
    assert!(msg.contains("cannot insert a non-DEFAULT value into column \"a\""));
    assert!(msg.contains("DETAIL: Column \"a\" is an identity column defined as GENERATED ALWAYS."));
    assert!(msg.contains("HINT:  Use OVERRIDING SYSTEM VALUE to override."));
}

#[test]
fn drop_if_exists_is_silent() {
    let mut e = Engine::new();
    // IF EXISTS on a missing object succeeds (SPG emits no NOTICE — a
    // recorded residual — but the statement completes cleanly).
    ok(&mut e, "DROP TABLE IF EXISTS nonexist_tbl");
    ok(&mut e, "CREATE TABLE t5(a int, b int)");
    ok(&mut e, "ALTER TABLE t5 DROP COLUMN IF EXISTS nope");
}
