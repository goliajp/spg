//! v7.39 (read01 round 170) — index metadata survives rebuild_indices.
//! The rebuild descriptor reconstructed every index via bare
//! `Index::new_btree(name, pos)`, silently dropping is_unique /
//! extra_column_positions / partial_predicate / expression /
//! included_columns / nulls_not_distinct — so the FIRST VACUUM (or any
//! delete-path rebuild) turned every UNIQUE INDEX into a plain one and
//! stopped enforcing it (probe-reproduced silent-wrong: duplicate keys
//! inserted freely after VACUUM). Under the MVCC gate every autovacuum
//! pass hit this. Locks the fixed behavior.

use spg_engine::Engine;

#[test]
fn unique_index_survives_vacuum() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u(id INT NOT NULL, x INT NOT NULL)")
        .unwrap();
    e.execute("CREATE UNIQUE INDEX u_x ON u (x)").unwrap();
    for i in 0..50 {
        e.execute(&format!("INSERT INTO u VALUES ({i}, {i})"))
            .unwrap();
    }
    assert!(e.execute("INSERT INTO u VALUES (100, 5)").is_err());
    e.execute("UPDATE u SET id = id + 1000").unwrap();
    e.execute("VACUUM u").unwrap();
    assert!(
        e.execute("INSERT INTO u VALUES (200, 5)").is_err(),
        "unique enforcement must survive a VACUUM-triggered rebuild"
    );
    // And the honest positive: a fresh key still inserts.
    e.execute("INSERT INTO u VALUES (300, 999)").unwrap();
}

#[test]
fn partial_unique_index_survives_vacuum() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p(id INT NOT NULL, x INT NOT NULL, live INT NOT NULL)")
        .unwrap();
    e.execute("CREATE UNIQUE INDEX p_x ON p (x) WHERE live = 1")
        .unwrap();
    e.execute("INSERT INTO p VALUES (1, 7, 1)").unwrap();
    e.execute("INSERT INTO p VALUES (2, 7, 0)").unwrap(); // outside predicate — ok
    assert!(e.execute("INSERT INTO p VALUES (3, 7, 1)").is_err());
    e.execute("UPDATE p SET id = id + 100").unwrap();
    e.execute("VACUUM p").unwrap();
    // Predicate semantics intact after the rebuild: in-predicate dup
    // still rejected, out-of-predicate dup still fine.
    assert!(e.execute("INSERT INTO p VALUES (4, 7, 1)").is_err());
    e.execute("INSERT INTO p VALUES (5, 7, 0)").unwrap();
}

#[test]
fn multicol_unique_survives_vacuum() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE m(a INT NOT NULL, b INT NOT NULL, pad INT NOT NULL)")
        .unwrap();
    e.execute("CREATE UNIQUE INDEX m_ab ON m (a, b)").unwrap();
    e.execute("INSERT INTO m VALUES (1, 1, 0)").unwrap();
    e.execute("INSERT INTO m VALUES (1, 2, 0)").unwrap();
    assert!(e.execute("INSERT INTO m VALUES (1, 1, 9)").is_err());
    e.execute("UPDATE m SET pad = pad + 1").unwrap();
    e.execute("VACUUM m").unwrap();
    assert!(
        e.execute("INSERT INTO m VALUES (1, 1, 9)").is_err(),
        "extra_column_positions must survive the rebuild"
    );
    e.execute("INSERT INTO m VALUES (2, 1, 0)").unwrap();
}
