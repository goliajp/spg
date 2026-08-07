//! v7.39 (round 642) — three things the partition graph got wrong, all
//! of them found after the differential oracle was repaired.
//!
//! A stray `CREATE PUBLICATION p543 FOR ALL TABLES` had been left on the
//! PG18 oracle by a round-543 test. With `wal_level = replica` and no
//! replica identity, that makes PG refuse every UPDATE and DELETE on an
//! ordinary table — so the corpus had been comparing SPG's real answers
//! against PG's refusals, and file 13 reported 11 differing lines every
//! round. Dropping the publication took it to 6. **The baseline was
//! wrong and its reproducibility was what made it look trustworthy.**
//!
//! What the repaired oracle then showed:
//!
//!   * **`pg_inherits.inhseqno` counts PARENTS, not siblings.** SPG
//!     numbered a parent's children 1, 2, 3. Measured both ways on
//!     PG18: a child of two parents gets 1 and 2, and two partitions of
//!     one parent BOTH get 1. A partition has one parent, so the answer
//!     is always 1. The code carried a comment claiming the old
//!     behaviour "matches PG's pg_inherits.inhseqno semantics".
//!
//!   * **`pg_class.relhassubclass` was hardcoded false.** A partitioned
//!     parent said it had no children while `pg_inherits` listed them
//!     two queries away. PG reads true for a partitioned parent and an
//!     inheritance parent alike.
//!
//!   * **A partition parent could not be dropped at all** (F28). The
//!     guard refused, saying PG needs an explicit CASCADE and that SPG
//!     had not wired it through. PG needs no such thing: a plain `DROP
//!     TABLE pp` takes pp and every partition, and so does the CASCADE
//!     spelling. Both are measured below. The practical effect was that
//!     `DROP TABLE IF EXISTS pp CASCADE` — the first line of any script
//!     that recreates a partitioned table — failed, and everything
//!     after it failed on the leftovers.
//!
//! Still open and NOT fixed here: `CREATE TABLE c () INHERITS (p)` is a
//! parse error. It is the last of file 13's divergences and it is a
//! feature, not a catalog repair. (The `pg_inherits` doc comment said
//! INHERITS was accepted as a no-op; it never was.)

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn partitioned(e: &mut Engine, parent: &str) {
    e.execute(&format!(
        "CREATE TABLE {parent} (k INT) PARTITION BY RANGE (k)"
    ))
    .unwrap();
    e.execute(&format!(
        "CREATE TABLE {parent}1 PARTITION OF {parent} FOR VALUES FROM (0) TO (10)"
    ))
    .unwrap();
    e.execute(&format!(
        "CREATE TABLE {parent}2 PARTITION OF {parent} FOR VALUES FROM (10) TO (20)"
    ))
    .unwrap();
}

#[test]
fn round642_inhseqno_counts_parents_not_siblings() {
    let mut e = Engine::new();
    partitioned(&mut e, "pp");
    assert_eq!(
        rows(
            &mut e,
            "SELECT c.relname, p.relname, i.inhseqno FROM pg_inherits i \
             JOIN pg_class c ON c.oid = i.inhrelid JOIN pg_class p ON p.oid = i.inhparent \
             ORDER BY c.relname"
        ),
        // Both 1 on PG. The second partition read 2 here.
        vec!["pp1|pp|1", "pp2|pp|1"]
    );
}

#[test]
fn round642_a_parent_says_it_has_children() {
    let mut e = Engine::new();
    partitioned(&mut e, "pp");
    e.execute("CREATE TABLE plain (a INT)").unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT relname, relhassubclass FROM pg_class \
             WHERE relname IN ('pp','pp1','plain') ORDER BY relname"
        ),
        vec!["plain|false", "pp|true", "pp1|false"]
    );
    // …and stops saying so once the children are gone.
    e.execute("DROP TABLE pp1").unwrap();
    e.execute("DROP TABLE pp2").unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT relhassubclass FROM pg_class WHERE relname = 'pp'"
        ),
        vec!["false"]
    );
}

#[test]
fn round642_dropping_a_partition_parent_takes_its_partitions() {
    let mut e = Engine::new();
    partitioned(&mut e, "pp");
    // Plain DROP, no CASCADE — which is what PG does.
    e.execute("DROP TABLE pp").unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT count(*) FROM pg_class WHERE relname IN ('pp','pp1','pp2')"
        ),
        vec!["0"]
    );
    // The CASCADE spelling behaves the same, and IF EXISTS in front of
    // it is the line every recreate script starts with.
    partitioned(&mut e, "pq");
    e.execute("DROP TABLE IF EXISTS pq CASCADE").unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT count(*) FROM pg_class WHERE relname IN ('pq','pq1','pq2')"
        ),
        vec!["0"]
    );
}

/// The drop walks depth-first, because a partition may itself be
/// partitioned and its children have to go before it does. SPG cannot
/// build that shape yet — writing this test is how the gap was found —
/// so what is pinned is the gap, and it flips when the feature lands.
///
/// Measured on PG18: `CREATE TABLE mid PARTITION OF top FOR VALUES FROM
/// (0) TO (10) PARTITION BY RANGE (j)` is accepted, `leaf` hangs off
/// `mid`, and `DROP TABLE top` removes all three.
#[test]
fn round642_sub_partitioning_is_still_a_parse_error() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE top (k INT, j INT) PARTITION BY RANGE (k)")
        .unwrap();
    let err = e
        .execute(
            "CREATE TABLE mid PARTITION OF top FOR VALUES FROM (0) TO (10) PARTITION BY RANGE (j)",
        )
        .expect_err("sub-partitioning parses on PG; when it parses here, fix this test");
    assert!(
        err.to_string().contains("syntax error"),
        "unexpected message: {err}"
    );
}

#[test]
fn round642_dropping_one_partition_leaves_the_rest() {
    let mut e = Engine::new();
    partitioned(&mut e, "pp");
    e.execute("DROP TABLE pp1").unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT relname FROM pg_class WHERE relname IN ('pp','pp1','pp2') ORDER BY relname"
        ),
        vec!["pp", "pp2"]
    );
}
