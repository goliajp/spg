//! Round 770 (F31 tranche 6, claims 151-189) — three fixes, all
//! PG18-measured:
//!
//! - #154: PG KEEPS the "read uncommitted" label (`SHOW
//!   transaction_isolation` answers it verbatim) and only BEHAVES as
//!   read committed; the old fold renamed the label too.
//! - #170: overlapping list partitions refuse with PG's sentence
//!   (`partition "b" would overlap partition "a"`), at both the
//!   PARTITION OF and ATTACH PARTITION sites.
//! - #174: `pg_partition_root` on a PLAIN table answers NULL (a
//!   partition parent stays its own root; a child walks up) — two
//!   contradictory comments both claimed their behaviour "matched
//!   PG"; only one did.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join(";"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn round770_read_uncommitted_keeps_its_label() {
    let mut e = Engine::new();
    e.execute("BEGIN ISOLATION LEVEL READ UNCOMMITTED").unwrap();
    assert_eq!(one(&mut e, "SHOW transaction_isolation"), "read uncommitted");
    e.execute("COMMIT").unwrap();
}

#[test]
fn round770_partition_overlap_speaks_pg() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p770 (id INT) PARTITION BY LIST (id)").unwrap();
    e.execute("CREATE TABLE p770a PARTITION OF p770 FOR VALUES IN (1, 2)").unwrap();
    let err = format!(
        "{}",
        e.execute("CREATE TABLE p770b PARTITION OF p770 FOR VALUES IN (2, 3)")
            .expect_err("overlap must refuse")
    );
    assert!(
        err.contains("partition \"p770b\" would overlap partition \"p770a\""),
        "{err}"
    );
}

#[test]
fn round770_partition_root_null_for_plain_tables() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p770 (id INT) PARTITION BY LIST (id)").unwrap();
    e.execute("CREATE TABLE p770a PARTITION OF p770 FOR VALUES IN (1)").unwrap();
    e.execute("CREATE TABLE plain770 (x INT)").unwrap();
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_partition_root('plain770') IS NULL, \
             pg_partition_root('p770a'), pg_partition_root('p770')"
        ),
        "true|p770|p770"
    );
}
