//! Round 692 — collation DERIVATION: what collation an expression has.
//!
//! Rounds 683–691 carried a collation from a COLUMN to the places that
//! compare it. That answers every key which IS a column, and the explicit
//! `COLLATE` clause. It cannot answer `ORDER BY upper(loc)`, because no
//! single column produced the value being compared.
//!
//! PG's rules, measured with `collation for (…)` on PG18 rather than read
//! out of anyone's source:
//!
//!   * a function result takes its arguments' collation
//!   * a literal has none, and yields to whatever it is combined with
//!   * an explicit `COLLATE` beats an implicit one
//!   * two DIFFERENT implicit collations do not pick a winner — the
//!     expression's collation is indeterminate, and using it errors
//!
//! That last rule is the one that would have been easy to get wrong by
//! being helpful. `ORDER BY a || b` where the two columns declare different
//! collations fails in PG with `collation mismatch between implicit
//! collations`, and SPG now fails with the same sentence. Quietly picking
//! one would have been F36's own defect in a new place.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seed(e: &mut Engine) {
    e.execute(
        "CREATE TABLE d692(a TEXT COLLATE \"en_US.utf8\", b TEXT COLLATE \"C\", plain TEXT)",
    )
    .unwrap();
    e.execute(
        "INSERT INTO d692 VALUES ('Zebra','Zebra','Zebra'),('apple','apple','apple'),\
         ('Banana','Banana','Banana'),('Ápple','Ápple','Ápple'),('cherry','cherry','cherry')",
    )
    .unwrap();
}

const EN_US: &str = "apple,Ápple,Banana,cherry,Zebra";
const BYTES: &str = "Banana,Zebra,apple,cherry,Ápple";

/// A function result and a concatenation both take the column's collation.
/// Both PG18-verified.
#[test]
fn round692_a_derived_key_sorts_by_the_columns_collation() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(rows(&mut e, "SELECT a FROM d692 ORDER BY upper(a)"), EN_US);
    assert_eq!(rows(&mut e, "SELECT a FROM d692 ORDER BY a || ''"), EN_US);
    assert_eq!(rows(&mut e, "SELECT a FROM d692 ORDER BY lower(a)"), EN_US);
}

/// And a key derived from a column that declares nothing keeps byte order,
/// so the pins above are not passing because everything became en_US.
///
/// The expected value is the byte order of the UPPERCASED text (`APPLE`,
/// `BANANA`, …, `ÁPPLE`), which is not the byte order of the values
/// themselves — the first version of this pin asserted the latter and was
/// wrong about what the query asks for, not about the engine.
#[test]
fn round692_a_derived_key_over_an_undeclared_column_keeps_bytes() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(
        rows(&mut e, "SELECT plain FROM d692 ORDER BY upper(plain)"),
        "apple,Banana,cherry,Zebra,Ápple"
    );
}

/// Two different implicit collations conflict, with PG18's sentence.
///
/// SPG could have picked one and produced an answer. PG does not, and the
/// reason to follow it here is that either choice silently changes the row
/// ORDER for a query the user believes is well-defined.
#[test]
fn round692_two_implicit_collations_conflict_rather_than_one_winning() {
    let mut e = Engine::new();
    seed(&mut e);
    let err = e
        .execute("SELECT a FROM d692 ORDER BY a || b")
        .expect_err("must not pick a winner");
    let msg = format!("{err}");
    assert!(
        msg.contains("collation mismatch between implicit collations"),
        "should read like PG's: {msg}"
    );
    assert!(msg.contains("en_US.utf8") && msg.contains('C'), "{msg}");
}

/// A literal yields rather than diluting: `a || 'lit'` keeps `a`'s
/// collation, so this is NOT a conflict.
#[test]
fn round692_a_literal_does_not_conflict() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(
        rows(&mut e, "SELECT a FROM d692 ORDER BY a || 'lit'"),
        EN_US
    );
}

/// An explicit `COLLATE` still wins over the derived one — the two features
/// compose, and this is the pin that would catch derivation overriding the
/// clause round 691 added.
#[test]
fn round692_an_explicit_collate_still_wins() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(
        rows(&mut e, "SELECT a FROM d692 ORDER BY a COLLATE \"C\""),
        BYTES
    );
}

/// A known difference from PG18, recorded rather than pinned as agreement:
/// on the oracle container `datcollate` is `en_US.utf8`, so a column that
/// declares nothing still gets en_US there. SPG's database default is C.
/// That is a DATABASE-level default, not a derivation rule, and it is the
/// same difference `ORDER BY plain` has always had.
#[test]
fn round692_the_database_default_is_c_and_that_is_the_remaining_difference() {
    let mut e = Engine::new();
    seed(&mut e);
    // Plain column and derived key agree with each other — the derivation
    // is consistent; it is the default underneath that differs from the
    // oracle's container.
    assert_eq!(rows(&mut e, "SELECT plain FROM d692 ORDER BY plain"), BYTES);
    assert_eq!(
        rows(&mut e, "SELECT plain FROM d692 ORDER BY upper(plain)"),
        "apple,Banana,cherry,Zebra,Ápple"
    );
}
