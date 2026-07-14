//! v7.39 (read01 round 48) — the named-constraint epic. SPG used to
//! discard the name a user gave a CHECK / UNIQUE / PRIMARY KEY constraint
//! and only recognise a synthesised one, so a constraint could be ADDed by
//! name but never DROPped by it. The schema now stores the name
//! (FILE_VERSION 60 constraint-name appendix), DROP resolves the stored
//! name first (falling back to the synthesised form for pre-v60 catalogs),
//! a duplicate name is rejected, and RENAME CONSTRAINT works.

use spg_engine::Engine;

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn named_constraints_round_trip() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE nc1(a int, b int)");
    // The verified bug: all three kinds used to ADD fine but never DROP.
    ok(&mut e, "ALTER TABLE nc1 ADD CONSTRAINT c_pos CHECK (a > 0)");
    ok(&mut e, "ALTER TABLE nc1 DROP CONSTRAINT c_pos");
    ok(&mut e, "ALTER TABLE nc1 ADD CONSTRAINT u_b UNIQUE (b)");
    ok(&mut e, "ALTER TABLE nc1 DROP CONSTRAINT u_b");
    ok(&mut e, "ALTER TABLE nc1 ADD CONSTRAINT pk_a PRIMARY KEY (a)");
    ok(&mut e, "ALTER TABLE nc1 DROP CONSTRAINT pk_a");
}

#[test]
fn duplicate_constraint_name_is_rejected() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE nc2(a int)");
    ok(&mut e, "ALTER TABLE nc2 ADD CONSTRAINT c1 CHECK (a > 0)");
    assert!(
        err(&mut e, "ALTER TABLE nc2 ADD CONSTRAINT c1 CHECK (a > 1)")
            .contains("constraint \"c1\" for relation \"nc2\" already exists")
    );
    // The name is taken across kinds, not just within one.
    assert!(
        err(&mut e, "ALTER TABLE nc2 ADD CONSTRAINT c1 UNIQUE (a)")
            .contains("constraint \"c1\" for relation \"nc2\" already exists")
    );
}

#[test]
fn rename_constraint() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE nc3(a int, b int)");
    ok(&mut e, "ALTER TABLE nc3 ADD CONSTRAINT c_pos CHECK (a > 0)");
    ok(&mut e, "ALTER TABLE nc3 RENAME CONSTRAINT c_pos TO c_positive");
    // The old name is gone, the new one drops.
    ok(&mut e, "ALTER TABLE nc3 DROP CONSTRAINT c_positive");
    // PG says "for table" here (DROP CONSTRAINT says "of relation").
    assert!(
        err(&mut e, "ALTER TABLE nc3 RENAME CONSTRAINT nope TO zz")
            .contains("constraint \"nope\" for table \"nc3\" does not exist")
    );
    // Renaming onto a taken name is rejected.
    ok(&mut e, "ALTER TABLE nc3 ADD CONSTRAINT k1 CHECK (a > 1)");
    ok(&mut e, "ALTER TABLE nc3 ADD CONSTRAINT k2 CHECK (b > 1)");
    assert!(
        err(&mut e, "ALTER TABLE nc3 RENAME CONSTRAINT k1 TO k2")
            .contains("constraint \"k2\" for relation \"nc3\" already exists")
    );
}

#[test]
fn inline_named_constraint_is_kept() {
    let mut e = Engine::new();
    // The inline CONSTRAINT <name> form in CREATE TABLE stores its name too.
    ok(
        &mut e,
        "CREATE TABLE nc4(a int, CONSTRAINT c_inline CHECK (a > 0))",
    );
    ok(&mut e, "ALTER TABLE nc4 DROP CONSTRAINT c_inline");
    // An unnamed CHECK is still reachable by its synthesised name.
    ok(&mut e, "CREATE TABLE nc5(a int, CHECK (a > 0))");
    ok(&mut e, "ALTER TABLE nc5 DROP CONSTRAINT nc5_a_check");
}
