//! v7.39.13 — `pg_index` stops guessing which constraint an index backs.
//!
//! Reported by sentori against 7.39.12. SPG builds a SINGLE-column
//! index for a composite constraint added by `ALTER TABLE` — the form
//! `pg_dump` emits — while the constraint records every column, so the
//! catalog matched them by PREFIX and reported the guess. Measured
//! against PG 18.6:
//!
//! ```text
//!                                PG 18.6                 SPG 7.39.12
//!   PK(a,b) then INDEX ix(a)   ix f/f/1, pkey t/t/2    BOTH t/t/1
//!   INDEX ix(a) then PK(a,b)   ix f/f/1, pkey t/t/2    ix only
//!   is ix actually unique?     no                      says yes
//!   PK(a,b,c) natts / name     3 / ck_pkey             1 / ck_a_pkey
//! ```
//!
//! The third line is the dangerous one, and it points the opposite way
//! from the defect 7.39.11 fixed. That one was a real key reporting NOT
//! unique — a false alarm. This is an index that accepts duplicates
//! reporting that it is unique: a migration guard that asserts
//! uniqueness before relying on it gets a yes and is wrong.
//!
//! Nothing prefix-matches now. A storage index reports its own columns
//! and its own uniqueness; each uniqueness constraint gets the row
//! PostgreSQL gives it; and a storage index whose columns EXACTLY equal
//! a constraint's is that constraint's index and carries its flags, so
//! an inline `PRIMARY KEY (a, b)` stays one row. `catalog_indexes` is
//! the single enumeration all five catalog surfaces walk — they had to
//! agree on the order and one of them said so in a comment, which is
//! how they came to agree on the same wrong answer.

use spg_engine::{Engine, QueryResult};

fn rows_of(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
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

const KEY_SHAPE: &str = "SELECT c.relname, i.indnatts::text, i.indisunique::text, \
     i.indisprimary::text FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
     WHERE i.indrelid = 'o'::regclass ORDER BY c.relname";

/// PG 18.6: `ck_pkey`, three columns, unique, primary — one row.
#[test]
fn a_composite_key_added_by_alter_reports_all_of_its_columns() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ck (a int NOT NULL, b int NOT NULL, c int NOT NULL)")
        .unwrap();
    e.execute("ALTER TABLE ck ADD PRIMARY KEY (a, b, c)")
        .unwrap();
    let got = rows_of(
        &mut e,
        "SELECT c.relname, i.indnatts::text, i.indisunique::text, i.indisprimary::text \
         FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
         WHERE i.indrelid = 'ck'::regclass AND i.indisprimary",
    );
    assert_eq!(
        got,
        alloc_rows(&[&["ck_pkey", "3", "true", "true"]]),
        "one primary key, named and sized as PostgreSQL names and sizes it"
    );
}

/// The dangerous direction: an index on a PREFIX of a composite key
/// accepts duplicates, and must not claim otherwise.
#[test]
fn an_index_on_a_prefix_of_the_key_is_not_unique_and_not_primary() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE o (a int NOT NULL, b int NOT NULL)")
        .unwrap();
    e.execute("ALTER TABLE o ADD PRIMARY KEY (a, b)").unwrap();
    e.execute("CREATE INDEX o_a_only ON o (a)").unwrap();
    // It really does accept two rows sharing `a` — the key is (a, b).
    e.execute("INSERT INTO o VALUES (1, 1), (1, 2)").unwrap();
    let got = rows_of(
        &mut e,
        "SELECT c.relname, i.indisunique::text FROM pg_index i \
         JOIN pg_class c ON c.oid = i.indexrelid \
         WHERE i.indrelid = 'o'::regclass AND c.relname = 'o_a_only'",
    );
    assert_eq!(
        got,
        alloc_rows(&[&["o_a_only", "false"]]),
        "an index that accepted a duplicate must not report indisunique"
    );
}

/// And exactly one row claims to be the primary key, whichever order
/// the index and the constraint were declared in.
#[test]
fn one_row_claims_the_primary_key_in_either_declaration_order() {
    for (label, first, second) in [
        (
            "constraint first",
            "ALTER TABLE o ADD PRIMARY KEY (a, b)",
            "CREATE INDEX o_a_only ON o (a)",
        ),
        (
            "index first",
            "CREATE INDEX o_a_only ON o (a)",
            "ALTER TABLE o ADD PRIMARY KEY (a, b)",
        ),
    ] {
        let mut e = Engine::new();
        e.execute("CREATE TABLE o (a int NOT NULL, b int NOT NULL)")
            .unwrap();
        e.execute(first).unwrap();
        e.execute(second).unwrap();
        let primaries = rows_of(
            &mut e,
            "SELECT count(*)::text FROM pg_index \
             WHERE indrelid = 'o'::regclass AND indisprimary",
        );
        assert_eq!(primaries, alloc_rows(&[&["1"]]), "{label}");
        // and the key that claims it covers both columns
        let shape = rows_of(&mut e, KEY_SHAPE);
        assert!(
            shape
                .iter()
                .any(|r| r[0] == "o_pkey" && r[1] == "2" && r[2] == "true" && r[3] == "true"),
            "{label}: no o_pkey over two columns in {shape:?}"
        );
    }
}

/// `pg_indexes` lists the same objects `pg_index` does. Two catalogs
/// disagreeing about one schema is its own defect.
#[test]
fn pg_indexes_lists_the_constraints_index_too() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE o (a int NOT NULL, b int NOT NULL)")
        .unwrap();
    e.execute("ALTER TABLE o ADD PRIMARY KEY (a, b)").unwrap();
    let listed = rows_of(
        &mut e,
        "SELECT indexname FROM pg_indexes WHERE tablename = 'o' ORDER BY indexname",
    );
    assert!(
        listed.iter().any(|r| r[0] == "o_pkey"),
        "pg_index has o_pkey and pg_indexes does not: {listed:?}"
    );
}

fn alloc_rows(want: &[&[&str]]) -> Vec<Vec<String>> {
    want.iter()
        .map(|r| r.iter().map(|s| (*s).to_string()).collect())
        .collect()
}
