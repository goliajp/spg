//! v7.39 (read01 commands/, round 47) — the rest of the DDL object-error
//! surface: duplicate-relation errors (CREATE TABLE / INDEX / VIEW /
//! SEQUENCE / TYPE), the RENAME / ALTER COLUMN / DROP CONSTRAINT
//! wordings, and a correctness fix — ALTER TABLE ... RENAME TO an
//! existing relation name is now rejected. Wording byte-locked vs PG18;
//! the wire maps these to 42P07 / 42710 / 42704 / 42703 / 42P01.
//!
//! PG's own wording is inconsistent and matched verbatim, not normalised:
//! DROP TABLE says "table" while ALTER says "relation"; RENAME COLUMN
//! omits the "of relation" qualifier that the ALTER COLUMN family carries;
//! DROP CONSTRAINT says "of relation" while ADD CONSTRAINT says "for
//! relation".

use spg_engine::Engine;

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn duplicate_relation_errors() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE r1(a int, b text)");
    assert!(err(&mut e, "CREATE TABLE r1(a int)").contains("relation \"r1\" already exists"));
    ok(&mut e, "CREATE INDEX r1_ix ON r1(a)");
    assert!(
        err(&mut e, "CREATE INDEX r1_ix ON r1(a)").contains("relation \"r1_ix\" already exists")
    );
    ok(&mut e, "CREATE SEQUENCE r_seq");
    assert!(err(&mut e, "CREATE SEQUENCE r_seq").contains("relation \"r_seq\" already exists"));
    ok(&mut e, "CREATE VIEW r_view AS SELECT a FROM r1");
    assert!(
        err(&mut e, "CREATE VIEW r_view AS SELECT a FROM r1")
            .contains("relation \"r_view\" already exists")
    );
    // A type is NOT a relation to PG — it keeps its own wording (42710).
    ok(&mut e, "CREATE TYPE r_enum AS ENUM ('x')");
    assert!(
        err(&mut e, "CREATE TYPE r_enum AS ENUM ('y')").contains("type \"r_enum\" already exists")
    );
}

#[test]
fn rename_to_existing_relation_is_rejected() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE r1(a int)");
    ok(&mut e, "CREATE TABLE r2(a int)");
    // Renaming onto its own name is an error in PG, not a no-op.
    assert!(err(&mut e, "ALTER TABLE r1 RENAME TO r1").contains("relation \"r1\" already exists"));
    assert!(err(&mut e, "ALTER TABLE r1 RENAME TO r2").contains("relation \"r2\" already exists"));
}

#[test]
fn rename_and_alter_column_wordings() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE r1(a int, b text)");
    // RENAME COLUMN: PG omits the "of relation" qualifier on the source.
    assert!(
        err(&mut e, "ALTER TABLE r1 RENAME COLUMN nope TO zz")
            .contains("column \"nope\" does not exist")
    );
    // …but carries it on the target-name collision.
    assert!(
        err(&mut e, "ALTER TABLE r1 RENAME COLUMN a TO b")
            .contains("column \"b\" of relation \"r1\" already exists")
    );
    // The ALTER COLUMN family all say "of relation".
    for sql in [
        "ALTER TABLE r1 ALTER COLUMN nope SET NOT NULL",
        "ALTER TABLE r1 ALTER COLUMN nope SET DEFAULT 1",
        "ALTER TABLE r1 ALTER COLUMN nope TYPE bigint",
    ] {
        assert!(
            err(&mut e, sql).contains("column \"nope\" of relation \"r1\" does not exist"),
            "{sql}"
        );
    }
}

#[test]
fn missing_relation_and_constraint_wordings() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE r1(a int)");
    // ALTER on a missing table says "relation" (DROP TABLE says "table").
    assert!(
        err(&mut e, "ALTER TABLE nope_tbl ADD COLUMN z int")
            .contains("relation \"nope_tbl\" does not exist")
    );
    assert!(
        err(&mut e, "ALTER TABLE nope_tbl RENAME TO r2")
            .contains("relation \"nope_tbl\" does not exist")
    );
    assert!(
        err(&mut e, "ALTER TABLE r1 DROP CONSTRAINT nope_con")
            .contains("constraint \"nope_con\" of relation \"r1\" does not exist")
    );
}
