//! v7.37.18 (18.7-18.15) — accept-and-no-op for the ALTER TABLE
//! subjects that pg_dump emits but SPG treats as N/A under its
//! single-tenant / single-owner / no-tablespace model. These all
//! need to parse without error so a PG dump can round-trip
//! through `psql` against SPG.

use spg_engine::Engine;

fn fresh() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)").unwrap();
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
    e.execute("ALTER TABLE t SET TABLESPACE pg_default").unwrap();
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
    e.execute("ALTER TABLE t SET REPLICA IDENTITY FULL").unwrap();
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

#[test]
fn alter_table_validate_constraint_accepted() {
    let mut e = fresh();
    e.execute("ALTER TABLE t VALIDATE CONSTRAINT some_fk").unwrap();
}
