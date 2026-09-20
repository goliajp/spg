//! 9.0.0 — `INSERT INTO <partitioned parent> … RETURNING` and
//! `… ON CONFLICT` were refused outright.
//!
//! The refusal said "route through the child explicitly", which asks
//! the caller to work out which partition a row lands in — the one
//! thing routing exists to answer. PostgreSQL 18.6 takes both.
//!
//! Measured there: the RETURNING rows come back in the statement's
//! ORIGINAL tuple order, interleaved across partitions, not grouped by
//! partition. Bucketing the tuples by destination — what the routing
//! did, one statement per child — loses that order and the per-tuple
//! attribution `ON CONFLICT DO NOTHING` needs, so those two shapes
//! route a tuple at a time. Everything else keeps the bucketed path.
//!
//! Two things had to be fixed under it:
//!
//!   * a partition copied the parent's uniqueness constraints but got
//!     no B-tree for them, so `pg_indexes` showed `<child>_pkey` while
//!     storage had none. The `ON CONFLICT` arbiter probes storage
//!     indexes, found nothing, answered "no conflict", and the row
//!     reached the uniqueness check — EVERY `ON CONFLICT` spelling on
//!     a partition raised `duplicate key value violates unique
//!     constraint` where PG upserts.
//!   * `ON CONFLICT DO UPDATE` refuses two tuples that would affect one
//!     row, and per-row routing makes each tuple its own statement, so
//!     the batch-local check never saw the pair. Comparing the target's
//!     key up front is enough: a partitioned table's unique constraint
//!     must include every partition-key column, so two tuples sharing a
//!     key always route to one partition.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    rows.into_iter()
        .map(|r| {
            r.values
                .iter()
                .map(spg_engine::eval::value_to_text)
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE TABLE ap (id int PRIMARY KEY, v text) PARTITION BY RANGE (id)",
        "CREATE TABLE ap1 PARTITION OF ap FOR VALUES FROM (0) TO (100)",
        "CREATE TABLE ap2 PARTITION OF ap FOR VALUES FROM (100) TO (200)",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    e
}

#[test]
fn returning_keeps_the_statements_own_tuple_order() {
    let mut e = seeded();
    // 150 and 120 go to ap2, 5 goes to ap1 — the order below is
    // interleaved on purpose, and PostgreSQL returns it unchanged.
    assert_eq!(
        rows(
            &mut e,
            "INSERT INTO ap VALUES (150,'b'),(5,'a'),(120,'c') RETURNING id, v"
        ),
        vec!["150|b".to_string(), "5|a".to_string(), "120|c".to_string()]
    );
}

#[test]
fn on_conflict_do_update_upserts_through_the_parent() {
    let mut e = seeded();
    e.execute("INSERT INTO ap VALUES (5,'a'),(150,'b')")
        .unwrap();
    assert_eq!(
        rows(
            &mut e,
            "INSERT INTO ap VALUES (5,'dup') ON CONFLICT (id) DO UPDATE SET v = 'upd' \
             RETURNING id, v"
        ),
        vec!["5|upd".to_string()]
    );
    assert_eq!(
        rows(&mut e, "SELECT id, v FROM ap ORDER BY id"),
        vec!["5|upd".to_string(), "150|b".to_string()]
    );
}

#[test]
fn a_partition_has_the_index_its_inherited_constraint_needs() {
    let mut e = seeded();
    e.execute("INSERT INTO ap VALUES (5,'a')").unwrap();
    // DO NOTHING has to see the conflict, which it can only do through
    // a storage index on the partition.
    e.execute("INSERT INTO ap VALUES (5,'x') ON CONFLICT (id) DO NOTHING")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM ap"),
        vec!["1".to_string()]
    );
    // And the same asked of the partition directly.
    e.execute("INSERT INTO ap1 VALUES (5,'y') ON CONFLICT (id) DO NOTHING")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM ap"),
        vec!["1".to_string()]
    );
}

#[test]
fn two_tuples_that_would_affect_one_row_are_refused() {
    let mut e = seeded();
    let got = format!(
        "{}",
        e.execute("INSERT INTO ap VALUES (7,'x'),(7,'y') ON CONFLICT (id) DO UPDATE SET v = 'z'")
            .unwrap_err()
    );
    assert!(
        got.contains("ON CONFLICT DO UPDATE command cannot affect row a second time"),
        "{got}"
    );
    // Distinct keys in one command are fine.
    assert_eq!(
        rows(
            &mut e,
            "INSERT INTO ap VALUES (8,'x'),(9,'y') ON CONFLICT (id) DO UPDATE SET v = 'z' \
             RETURNING id"
        ),
        vec!["8".to_string(), "9".to_string()]
    );
}
