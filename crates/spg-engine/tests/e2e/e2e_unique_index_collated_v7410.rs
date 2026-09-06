//! v7.40.10 — a single-column `CREATE UNIQUE INDEX` on a text column
//! did not enforce uniqueness under the collation SPG ships.
//!
//! Reported against 7.40.9 as the most severe class in the
//! correspondence: silent, persisted, duplicated rows, with
//! `indisunique` reading `true` throughout.
//!
//! Boundary, measured on the published 7.40.9 image (database collation
//! `en_US.utf8`), two rows with the same value inserted by two separate
//! statements:
//!
//! ```text
//!   int / date single column                    rejected
//!   text COLLATE "C" column                     rejected
//!   index declared (t COLLATE "C")              rejected
//!   text, multi-column                          rejected
//!   UNIQUE column constraint                    rejected
//!   CREATE UNIQUE INDEX on a text column        ACCEPTED — two rows
//!   the same on varchar(32)                     ACCEPTED — two rows
//! ```
//!
//! Two more readings placed it exactly. Creating the index over
//! existing duplicates is refused, and two duplicates inside ONE
//! statement are refused — so the flag is set and the batch-internal
//! check works. What fails is the probe against rows already committed.
//!
//! The fast path in `enforce_unique_index_inserts` asks the index's
//! B-tree. For a locale-collated column that tree is keyed by ICU sort
//! keys the engine produces, so a probe built from the raw value asks a
//! question those keys cannot answer, and an empty answer reads as "no
//! conflict". `enforce_uniqueness_inserts` — the CONSTRAINT path —
//! already declines such an index and folds the whole table, which is
//! why every constraint spelling enforced. One rule, two sites, and
//! only one of them had heard it.
//!
//! **The collation is declared here rather than inherited.** Every
//! fixture in the tree runs under `C`, where the tree is keyed by bytes
//! and the probe is answerable, so a test that took the default would
//! have passed against the defect. The gate's shipped-collation run
//! sets `SPG_E2E_DB_COLLATION` for the SERVER suite; this file makes
//! its own condition so it bites in every run of the engine suite too.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn shipped_collation_engine() -> Engine {
    let mut eng = Engine::new();
    assert!(
        eng.declare_database_collation("en_US.utf8")
            .expect("en_US.utf8 is a collation this build performs"),
        "the collation must take effect before any table exists"
    );
    eng
}

fn count(eng: &mut Engine, table: &str) -> i64 {
    match eng
        .execute(&format!("SELECT count(*) FROM {table}"))
        .expect("count")
    {
        QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            Value::BigInt(n) => n,
            ref other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

/// Two statements, because one statement was never the failing case.
fn insert_twice(eng: &mut Engine, first: &str, second: &str) -> Result<(), String> {
    eng.execute(first).map_err(|e| format!("{e:?}"))?;
    eng.execute(second).map_err(|e| format!("{e:?}"))?;
    Ok(())
}

#[test]
fn a_single_column_unique_index_on_text_rejects_a_duplicate() {
    let mut eng = shipped_collation_engine();
    eng.execute("CREATE TABLE u (id INT, email TEXT)").unwrap();
    eng.execute("CREATE UNIQUE INDEX u_email ON u (email)")
        .unwrap();
    let out = insert_twice(
        &mut eng,
        "INSERT INTO u VALUES (1, 'a@x')",
        "INSERT INTO u VALUES (2, 'a@x')",
    );
    assert!(
        out.is_err(),
        "the second row duplicates the first and must be refused"
    );
    assert_eq!(count(&mut eng, "u"), 1, "and must not be stored");
}

#[test]
fn the_same_on_varchar() {
    let mut eng = shipped_collation_engine();
    eng.execute("CREATE TABLE v (id INT, t VARCHAR(32))")
        .unwrap();
    eng.execute("CREATE UNIQUE INDEX v_t ON v (t)").unwrap();
    let out = insert_twice(
        &mut eng,
        "INSERT INTO v VALUES (1, 'a')",
        "INSERT INTO v VALUES (2, 'a')",
    );
    assert!(out.is_err());
    assert_eq!(count(&mut eng, "v"), 1);
}

/// The value that differs only by collation weight must still be
/// allowed: this is a UNIQUE index, not a case-insensitive one, and
/// `en_US.utf8` distinguishes `a` from `A`.
#[test]
fn a_value_that_is_merely_close_is_not_a_duplicate() {
    let mut eng = shipped_collation_engine();
    eng.execute("CREATE TABLE w (id INT, t TEXT)").unwrap();
    eng.execute("CREATE UNIQUE INDEX w_t ON w (t)").unwrap();
    insert_twice(
        &mut eng,
        "INSERT INTO w VALUES (1, 'a')",
        "INSERT INTO w VALUES (2, 'A')",
    )
    .expect("'a' and 'A' are different values under en_US.utf8");
    assert_eq!(count(&mut eng, "w"), 2);
}

/// The readings that placed the defect, kept so a future change cannot
/// move it back one step and go unnoticed.
#[test]
fn the_two_checks_that_already_worked_still_do() {
    let mut eng = shipped_collation_engine();
    eng.execute("CREATE TABLE d (id INT, t TEXT)").unwrap();
    eng.execute("INSERT INTO d VALUES (1,'a'),(2,'a')").unwrap();
    assert!(
        eng.execute("CREATE UNIQUE INDEX d_t ON d (t)").is_err(),
        "creating the index over existing duplicates is refused"
    );

    let mut eng2 = shipped_collation_engine();
    eng2.execute("CREATE TABLE b (id INT, t TEXT)").unwrap();
    eng2.execute("CREATE UNIQUE INDEX b_t ON b (t)").unwrap();
    assert!(
        eng2.execute("INSERT INTO b VALUES (1,'a'),(2,'a')")
            .is_err(),
        "two duplicates in ONE statement are refused"
    );
    assert_eq!(count(&mut eng2, "b"), 0);
}

/// The neighbouring shapes, which enforced throughout. A fix that
/// reaches the collated case by disabling the fast path everywhere
/// would leave these correct and slow; a fix that breaks them would be
/// caught here.
#[test]
fn the_shapes_that_always_enforced_still_do() {
    let mut eng = shipped_collation_engine();
    eng.execute("CREATE TABLE n (id INT, k INT)").unwrap();
    eng.execute("CREATE UNIQUE INDEX n_k ON n (k)").unwrap();
    assert!(
        insert_twice(
            &mut eng,
            "INSERT INTO n VALUES (1, 7)",
            "INSERT INTO n VALUES (2, 7)"
        )
        .is_err(),
        "int"
    );

    eng.execute("CREATE TABLE m (id INT, a TEXT, b TEXT)")
        .unwrap();
    eng.execute("CREATE UNIQUE INDEX m_ab ON m (a, b)").unwrap();
    assert!(
        insert_twice(
            &mut eng,
            "INSERT INTO m VALUES (1,'a','b')",
            "INSERT INTO m VALUES (2,'a','b')"
        )
        .is_err(),
        "text, multi-column"
    );

    eng.execute("CREATE TABLE c (id INT, e TEXT UNIQUE)")
        .unwrap();
    assert!(
        insert_twice(
            &mut eng,
            "INSERT INTO c VALUES (1,'a@x')",
            "INSERT INTO c VALUES (2,'a@x')"
        )
        .is_err(),
        "UNIQUE column constraint"
    );
}
