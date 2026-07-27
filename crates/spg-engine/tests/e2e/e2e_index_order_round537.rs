//! v7.39 (round 537) — an index key's ordering clause survives.
//!
//! Round 536 measured this and recorded it rather than fixing it,
//! because it needed the parser, the storage record, the catalog codec
//! and a FILE_VERSION bump — a round of its own, which this is.
//!
//!     CREATE INDEX i ON t (a DESC NULLS LAST)
//!     PG18  indexdef … (a DESC NULLS LAST)      SPG  … (a)
//!
//! SPG's index does not scan in a direction — column ordering is
//! intrinsic to the storage — so this changes no lookup. What it
//! changes is that `pg_indexes.indexdef` reproduces the DDL again: a
//! dump kept the clause, and a schema diff stopped reporting drift on
//! every index that had one.
//!
//! PG prints only what is NOT the default, and the nulls default flips
//! with the direction — LAST for ascending, FIRST for descending. All
//! eight spellings were measured before the rule was written:
//!
//!     (a)                    (a)              (a ASC NULLS LAST)   (a)
//!     (a ASC)                (a)              (a ASC NULLS FIRST)  (a NULLS FIRST)
//!     (a DESC)               (a DESC)         (a DESC NULLS FIRST) (a DESC)
//!     (a DESC NULLS LAST)    (a DESC NULLS LAST)

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT, c TEXT)").unwrap();
    e
}

fn def(e: &mut Engine, name: &str) -> String {
    let sql = format!("SELECT indexdef FROM pg_indexes WHERE indexname = '{name}'");
    match e.execute(&sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => {
            let full = spg_engine::eval::value_to_text(&rows[0].values[0]);
            full.split("USING btree ")
                .nth(1)
                .unwrap_or(&full)
                .to_string()
        }
        other => panic!("{sql}: {other:?}"),
    }
}

/// Every spelling, against its PG18 reading.
#[test]
fn round537_index_order_is_reproduced_pgs_way() {
    let mut e = engine();
    for (i, (clause, expect)) in [
        ("a", "(a)"),
        ("a ASC", "(a)"),
        ("a DESC", "(a DESC)"),
        // NULLS LAST is the ascending default, so neither word prints.
        ("a ASC NULLS LAST", "(a)"),
        ("a ASC NULLS FIRST", "(a NULLS FIRST)"),
        // …and NULLS FIRST is the descending one.
        ("a DESC NULLS FIRST", "(a DESC)"),
        ("a DESC NULLS LAST", "(a DESC NULLS LAST)"),
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("ix{i}");
        e.execute(&format!("CREATE INDEX {name} ON t ({clause})"))
            .unwrap_or_else(|err| panic!("({clause}): {err}"));
        assert_eq!(def(&mut e, &name), expect, "CREATE INDEX … ({clause})");
    }
}

/// A UNIQUE index carries it too.
#[test]
fn round537_unique_index_keeps_its_order() {
    let mut e = engine();
    e.execute("CREATE UNIQUE INDEX u ON t (a DESC)").unwrap();
    assert_eq!(def(&mut e, "u"), "(a DESC)");
}

/// v7.39 (round 538) — an explicit COLLATE on the key prints, and
/// prints BEFORE the direction.
///
/// Measured: PG shows `(a COLLATE "C")` even on a C-collation database,
/// because a named collation and the one a column inherits are
/// different objects even where they sort identically. SPG orders text
/// by bytes — the C collation — so honouring the clause changes no
/// comparison; reproducing it is the whole job.
#[test]
fn round538_explicit_collate_on_the_key_prints() {
    let mut e = engine();
    e.execute(r#"CREATE INDEX cx ON t (c COLLATE "C")"#).unwrap();
    assert_eq!(def(&mut e, "cx"), r#"(c COLLATE "C")"#);
    // Before the direction, as PG orders the two.
    e.execute(r#"CREATE INDEX cy ON t (c COLLATE "C" DESC)"#)
        .unwrap();
    assert_eq!(def(&mut e, "cy"), r#"(c COLLATE "C" DESC)"#);
    // No clause, nothing printed.
    e.execute("CREATE INDEX cz ON t (c)").unwrap();
    assert_eq!(def(&mut e, "cz"), "(c)");
}

/// A locale collation is still refused rather than absorbed — SPG would
/// sort differently from PG and says so.
#[test]
fn round538_locale_collation_is_still_refused() {
    let mut e = engine();
    assert!(
        e.execute(r#"CREATE INDEX bad ON t (c COLLATE "en_US")"#)
            .is_err()
    );
}

/// It survives a reload, which is what the FILE_VERSION bump is for.
#[test]
fn round537_index_order_survives_a_round_trip() {
    let mut e = engine();
    e.execute("CREATE INDEX r ON t (a DESC NULLS LAST)").unwrap();
    e.execute(r#"CREATE INDEX rc ON t (c COLLATE "C" DESC)"#)
        .unwrap();
    let before = def(&mut e, "r");
    let before_c = def(&mut e, "rc");
    let snapshot = e.catalog().serialize();
    let mut back =
        Engine::restore(spg_storage::Catalog::deserialize(&snapshot).expect("roundtrip"));
    assert_eq!(def(&mut back, "r"), before);
    assert_eq!(before, "(a DESC NULLS LAST)");
    assert_eq!(def(&mut back, "rc"), before_c);
    assert_eq!(before_c, r#"(c COLLATE "C" DESC)"#);
}
