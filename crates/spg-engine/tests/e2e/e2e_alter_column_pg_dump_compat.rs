//! v7.37.18 (18.4 / 18.5 / 18.6 / 18.16) — accept-and-no-op for
//! ALTER COLUMN sub-subjects pg_dump emits but SPG treats as
//! N/A:
//!   - SET STATISTICS <n>     (18.4 — planner stats budget)
//!   - SET COMPRESSION <m>    (18.5 — pglz/lz4/zstd; SPG storage
//!                                    is engine-managed)
//!   - SET STORAGE <m>        (18.6 — plain/external/extended/main;
//!                                    SPG uses one in-line tier)
//!   - ALTER CONSTRAINT … {DEFERRABLE | INITIALLY DEFERRED}
//!                            (18.16 — SPG enforces immediately)

use spg_engine::Engine;

fn fresh() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)")
        .unwrap();
    e
}

#[test]
fn alter_column_set_statistics_accepted() {
    let mut e = fresh();
    e.execute("ALTER TABLE t ALTER COLUMN name SET STATISTICS 1000")
        .unwrap();
    e.execute("ALTER TABLE t ALTER COLUMN name SET STATISTICS -1")
        .unwrap();
}

#[test]
fn alter_column_set_compression_accepted() {
    let mut e = fresh();
    for codec in ["pglz", "lz4", "zstd", "default"] {
        let sql = format!("ALTER TABLE t ALTER COLUMN name SET COMPRESSION {codec}");
        e.execute(&sql).unwrap();
    }
}

#[test]
fn alter_column_set_storage_accepted() {
    let mut e = fresh();
    for mode in ["plain", "external", "extended", "main"] {
        let sql = format!("ALTER TABLE t ALTER COLUMN name SET STORAGE {mode}");
        e.execute(&sql).unwrap();
    }
}

#[test]
fn alter_constraint_deferrable_accepted() {
    let mut e = fresh();
    e.execute("CREATE TABLE p (id INT NOT NULL PRIMARY KEY)")
        .unwrap();
    e.execute(
        "CREATE TABLE c (id INT NOT NULL, pid INT NOT NULL, \
         CONSTRAINT c_pid_fk FOREIGN KEY (pid) REFERENCES p(id))",
    )
    .unwrap();
    // PG dumps may emit ALTER CONSTRAINT to toggle deferral.
    // SPG enforces immediately; accept-and-no-op.
    e.execute("ALTER TABLE c ALTER CONSTRAINT c_pid_fk DEFERRABLE INITIALLY DEFERRED")
        .unwrap();
    e.execute("ALTER TABLE c ALTER CONSTRAINT c_pid_fk NOT DEFERRABLE")
        .unwrap();
}
