//! v7.37.18 (18.7-18.15) — accept-and-no-op for the ALTER TABLE
//! subjects that pg_dump emits but SPG treats as N/A under its
//! single-tenant / single-owner / no-tablespace model. These all
//! need to parse without error so a PG dump can round-trip
//! through `psql` against SPG.

use spg_engine::Engine;

fn fresh() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)")
        .unwrap();
    e
}

#[test]
fn alter_table_owner_to_accepted() {
    let mut e = fresh();
    e.execute("ALTER TABLE t OWNER TO postgres").unwrap();
}

#[test]
fn alter_table_set_schema_accepted() {
    let mut e = fresh();
    e.execute("ALTER TABLE t SET SCHEMA public").unwrap();
}

#[test]
fn alter_table_set_tablespace_accepted() {
    let mut e = fresh();
    e.execute("ALTER TABLE t SET TABLESPACE pg_default")
        .unwrap();
}

#[test]
fn alter_table_set_logged_unlogged_accepted() {
    let mut e = fresh();
    e.execute("ALTER TABLE t SET LOGGED").unwrap();
    e.execute("ALTER TABLE t SET UNLOGGED").unwrap();
}

#[test]
fn alter_table_set_storage_parameters_accepted() {
    let mut e = fresh();
    e.execute("ALTER TABLE t SET (fillfactor = 70)").unwrap();
}

#[test]
fn alter_table_set_replica_identity_accepted() {
    let mut e = fresh();
    e.execute("ALTER TABLE t SET REPLICA IDENTITY FULL")
        .unwrap();
    e.execute("ALTER TABLE t SET REPLICA IDENTITY DEFAULT")
        .unwrap();
}

#[test]
fn alter_table_inherit_no_inherit_accepted() {
    let mut e = fresh();
    e.execute("CREATE TABLE parent (id INT)").unwrap();
    e.execute("ALTER TABLE t INHERIT parent").unwrap();
    e.execute("ALTER TABLE t NO INHERIT parent").unwrap();
}

#[test]
fn alter_table_cluster_on_accepted() {
    let mut e = fresh();
    e.execute("CREATE INDEX ix_t_id ON t(id)").unwrap();
    e.execute("ALTER TABLE t CLUSTER ON ix_t_id").unwrap();
    e.execute("ALTER TABLE t SET WITHOUT CLUSTER").unwrap();
}

/// v7.39 (round 652) — this used to assert that validating a constraint
/// that does not exist SUCCEEDS, under the name `..._accepted`. PG errors,
/// and SPG now does too. The test was the F31 shape: it pinned a gap as a
/// rule, so closing the gap is what made it red.
#[test]
fn alter_table_validate_constraint_rejects_an_unknown_name() {
    let mut e = fresh();
    let err = e
        .execute("ALTER TABLE t VALIDATE CONSTRAINT some_fk")
        .expect_err("PG: constraint \"some_fk\" of relation \"t\" does not exist");
    assert!(
        format!("{err}").contains("constraint \"some_fk\" of relation \"t\" does not exist"),
        "{err}"
    );
}

/// Same shape, measured the same way: OWNER TO and CLUSTER ON swallowed
/// the whole statement, so a name that does not exist passed silently.
/// SPG stays single-owner and unclustered — only the refusal is new.
#[test]
fn alter_table_owner_and_cluster_reject_unknown_names() {
    let mut e = fresh();
    let err = e
        .execute("ALTER TABLE t OWNER TO nonexistent_role")
        .expect_err("PG: role \"nonexistent_role\" does not exist");
    assert!(
        format!("{err}").contains("role \"nonexistent_role\" does not exist"),
        "{err}"
    );

    let err = e
        .execute("ALTER TABLE t CLUSTER ON no_such_index")
        .expect_err("PG: index \"no_such_index\" for table \"t\" does not exist");
    assert!(
        format!("{err}").contains("index \"no_such_index\" for table \"t\" does not exist"),
        "{err}"
    );

    // A role and an index that DO exist stay no-ops, as before.
    e.execute("CREATE USER u1").unwrap();
    e.execute("ALTER TABLE t OWNER TO u1").unwrap();
    e.execute("CREATE INDEX ix_t_id ON t(id)").unwrap();
    e.execute("ALTER TABLE t CLUSTER ON ix_t_id").unwrap();
}

/// PG names the parent first. SPG had the two relations the other way
/// round, so a client matching on the message blamed the wrong one.
#[test]
fn alter_table_no_inherit_names_the_parent_first() {
    let mut e = fresh();
    e.execute("CREATE TABLE parent (id INT)").unwrap();
    let err = e
        .execute("ALTER TABLE t NO INHERIT parent")
        .expect_err("PG: relation \"parent\" is not a parent of relation \"t\"");
    assert!(
        format!("{err}").contains("relation \"parent\" is not a parent of relation \"t\""),
        "{err}"
    );
}
