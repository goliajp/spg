//! v7.39.12 — a composite PRIMARY KEY added by `ALTER TABLE` is
//! findable in `pg_index` again.
//!
//! Reported by sentori against 7.39.11, and it is a regression this
//! project shipped. On their own dump, tables with a findable primary
//! key went from 27 of 27 to 20 of 27, and the seven that vanished are
//! the ones whose primary key is composite:
//!
//! ```text
//!   ALTER TABLE t ADD PRIMARY KEY (a, b)
//!                     7.39.10        7.39.11       PG 18
//!     indisprimary       t              f            t
//!     indisunique        f              f            t
//! ```
//!
//! v7.39.11 closed the single-column case by reading the flags off the
//! CONSTRAINT instead of guessing from the index's name — and matched
//! the constraint's columns to the index's by equality. SPG builds a
//! single-column index for a composite constraint added by `ALTER
//! TABLE`, which is the form `pg_dump` emits, so equality could never
//! match and both flags came back false. The name-guessing that was
//! removed happened to get `indisprimary` right for exactly this case.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows")
    };
    rows.iter()
        .map(|r| {
            r.values
                .iter()
                .map(spg_engine::eval::value_to_text)
                .collect()
        })
        .collect()
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE TABLE one (a int NOT NULL, b int NOT NULL)",
        "ALTER TABLE one ADD CONSTRAINT one_pkey PRIMARY KEY (a)",
        "CREATE TABLE two (a int NOT NULL, b int NOT NULL)",
        "ALTER TABLE two ADD CONSTRAINT two_pkey PRIMARY KEY (a, b)",
        "CREATE TABLE inl (a int NOT NULL, b int NOT NULL, PRIMARY KEY (a, b))",
    ] {
        e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"));
    }
    e
}

const FLAGS: &str = "SELECT c.relname, i.indisprimary, i.indisunique \
                     FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
                     ORDER BY c.relname";

#[test]
fn every_table_has_exactly_one_index_marked_primary() {
    // The shape their dump measures: one findable primary key per
    // table, three tables, three rows. It was two.
    let mut e = seeded();
    let r = rows(&mut e, "SELECT count(*) FROM pg_index WHERE indisprimary");
    assert_eq!(r[0][0], "3");
}

#[test]
fn a_composite_key_added_by_alter_table_is_primary_and_unique() {
    let mut e = seeded();
    let r = rows(&mut e, FLAGS);
    let two: Vec<&Vec<String>> = r.iter().filter(|x| x[0].starts_with("two")).collect();
    assert_eq!(two.len(), 1, "one index backs the constraint: {r:?}");
    assert_eq!(two[0][1], "true", "indisprimary");
    assert_eq!(two[0][2], "true", "indisunique");
}

#[test]
fn the_single_column_case_v7_39_11_closed_stays_closed() {
    let mut e = seeded();
    let r = rows(&mut e, FLAGS);
    let one: Vec<&Vec<String>> = r.iter().filter(|x| x[0].starts_with("one")).collect();
    assert_eq!(one[0][1], "true");
    assert_eq!(one[0][2], "true");
}

#[test]
fn an_inline_composite_marks_the_composite_index_and_not_the_other() {
    // SPG builds two indexes for the inline spelling — which sentori
    // and this project both still have open. What must hold is that
    // the one covering the whole key is the primary one and the one
    // covering a single non-leading column is not.
    let mut e = seeded();
    let r = rows(&mut e, FLAGS);
    let inl: Vec<&Vec<String>> = r.iter().filter(|x| x[0].starts_with("inl")).collect();
    assert_eq!(inl.len(), 2, "the known-open double row: {r:?}");
    let primary: Vec<&&Vec<String>> = inl.iter().filter(|x| x[1] == "true").collect();
    assert_eq!(primary.len(), 1, "exactly one of the pair is primary");
    assert!(
        primary[0][0].contains("_a_"),
        "the composite index is the one that covers the key: {inl:?}"
    );
}

#[test]
fn a_plain_index_over_a_constrained_column_is_not_unique() {
    // The negative control for the prefix rule: an index on a column
    // that no constraint covers must stay plain. `inl_b_pkey_0_1`
    // covers column b alone, and `PRIMARY KEY (a, b)` does not START
    // with (b), so the prefix does not match it.
    let mut e = seeded();
    let r = rows(&mut e, FLAGS);
    let b_only: Vec<&Vec<String>> = r.iter().filter(|x| x[0].contains("_b_")).collect();
    assert_eq!(b_only.len(), 1);
    assert_eq!(b_only[0][1], "false", "indisprimary");
    assert_eq!(b_only[0][2], "false", "indisunique");
}
