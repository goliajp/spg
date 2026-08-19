//! 7.38.3 — the inline `CHECK (…)` on `ALTER TABLE … ADD COLUMN`.
//!
//! sentori drop-in status §2.2: the inline form was accepted and
//! registered nothing — `pg_constraint` held no row and a violating
//! INSERT was let in. The parser had always put the predicate on the
//! ColumnDef; the ADD COLUMN path never read it. The separate
//! `ADD CONSTRAINT` spelling was enforced all along, which is why
//! three migrations using that form never noticed.

use spg_engine::Engine;

#[test]
fn pin_v7383_add_column_inline_check_is_registered() {
    // sentori 2.2: the inline form was accepted and registered nothing —
    // pg_constraint empty, violating INSERT allowed. PG's semantics,
    // matched here: the constraint exists under <table>_<column>_check
    // and is validated against the rows already present.
    let mut e = Engine::new();
    e.execute("CREATE TABLE ac (id INT PRIMARY KEY)").unwrap();
    e.execute("ALTER TABLE ac ADD COLUMN env TEXT CHECK (env IN ('a','b'))")
        .unwrap();
    let err = e
        .execute("INSERT INTO ac VALUES (1, 'zzz')")
        .unwrap_err()
        .to_string();
    assert!(err.contains("ac_env_check"), "{err}");
    e.execute("INSERT INTO ac VALUES (2, 'a')").unwrap();

    // Validated against existing rows: the NULL backfill fails a
    // NOT NULL test, and PG refuses the whole statement.
    let mut e2 = Engine::new();
    e2.execute("CREATE TABLE av (id INT)").unwrap();
    e2.execute("INSERT INTO av VALUES (1)").unwrap();
    let err = e2
        .execute("ALTER TABLE av ADD COLUMN e TEXT CHECK (e IS NOT NULL)")
        .unwrap_err()
        .to_string();
    assert!(err.contains("violated by some row"), "{err}");
    // And the refused column did not stay behind.
    e2.execute("INSERT INTO av VALUES (2)").unwrap();
    // A DEFAULT that satisfies the check makes the same ALTER succeed.
    e2.execute("ALTER TABLE av ADD COLUMN f TEXT DEFAULT 'z' CHECK (f IS NOT NULL)")
        .unwrap();
}
