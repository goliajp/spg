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

/// v7.39.13 — this row asserted "one index backs the constraint" and
/// that it carried both flags, which was the PREFIX GUESS this version
/// removed: the single-column index SPG builds for `ALTER TABLE ADD
/// PRIMARY KEY (a, b)` is not the key, and saying it was is how an
/// index that accepts duplicates came to report `indisunique`.
///
/// The constraint has its own row now, `two_pkey` over both columns,
/// and SPG's own probe index reports itself. What still has to hold —
/// and is what their dump measures — is that exactly ONE row per table
/// claims the primary key.
#[test]
fn a_composite_key_added_by_alter_table_is_primary_and_unique() {
    let mut e = seeded();
    let r = rows(&mut e, FLAGS);
    let two: Vec<&Vec<String>> = r.iter().filter(|x| x[0].starts_with("two")).collect();
    let primary: Vec<&&Vec<String>> = two.iter().filter(|x| x[1] == "true").collect();
    assert_eq!(primary.len(), 1, "exactly one primary among {two:?}");
    assert_eq!(primary[0][0], "two_pkey", "PostgreSQL's name for it");
    assert_eq!(primary[0][2], "true", "indisunique");
    for row in &two {
        if row[0] != "two_pkey" {
            assert_eq!(
                row[2], "false",
                "a probe index over a prefix of the key accepts duplicates: {row:?}"
            );
        }
    }
}

#[test]
fn the_single_column_case_v7_39_11_closed_stays_closed() {
    let mut e = seeded();
    let r = rows(&mut e, FLAGS);
    let one: Vec<&Vec<String>> = r.iter().filter(|x| x[0].starts_with("one")).collect();
    assert_eq!(one[0][1], "true");
    assert_eq!(one[0][2], "true");
}

/// v7.39.13 — this said "SPG builds two indexes for the inline
/// spelling — which sentori and this project both still have open",
/// and asserted the pair. It is closed: storage records which index a
/// constraint built, so the probe index over a non-leading column is no
/// longer a catalog object and the constraint's own index carries
/// PostgreSQL's name for it. PG 18.6 shows exactly `inl_pkey`.
#[test]
fn an_inline_composite_reports_one_index_named_as_postgresql_names_it() {
    let mut e = seeded();
    let r = rows(&mut e, FLAGS);
    let inl: Vec<&Vec<String>> = r.iter().filter(|x| x[0].starts_with("inl")).collect();
    assert_eq!(inl.len(), 1, "one index per constraint, as PG has: {r:?}");
    assert_eq!(inl[0][0], "inl_pkey");
    assert_eq!(inl[0][1], "true", "indisprimary");
    assert_eq!(inl[0][2], "true", "indisunique");
}

/// v7.39.13 — the same intent, on an index that still exists.
///
/// This used to reach for `inl_b_pkey_0_1`, SPG's own probe index over
/// a non-leading key column, which the catalog no longer shows. A USER
/// index over a constrained column is the case that matters, and it
/// must stay plain: it accepts duplicates, and 7.39.12 reported it
/// unique.
#[test]
fn a_user_index_over_a_constrained_column_is_not_unique() {
    let mut e = seeded();
    e.execute("CREATE INDEX two_b_plain ON two (b)").unwrap();
    // It really does accept two rows sharing b — the key is (a, b).
    e.execute("INSERT INTO two VALUES (1, 9), (2, 9)").unwrap();
    let r = rows(&mut e, FLAGS);
    let plain: Vec<&Vec<String>> = r.iter().filter(|x| x[0] == "two_b_plain").collect();
    assert_eq!(plain.len(), 1, "{r:?}");
    assert_eq!(plain[0][1], "false", "indisprimary");
    assert_eq!(plain[0][2], "false", "indisunique");
}
