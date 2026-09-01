//! v7.39.11 — `pg_index` said a PRIMARY KEY was not unique, and a bare
//! `CREATE UNIQUE INDEX` grew a constraint PostgreSQL does not create.
//!
//! Reported by sentori against the published 7.39.10 image as the
//! PostgreSQL twin of the `SHOW INDEX` defect that version fixed for
//! MySQL. Measured here against PostgreSQL 18.6, same DDL:
//!
//! ```text
//!   DDL                          PG 18                  SPG 7.39.10
//!   a int PRIMARY KEY            indisprimary=t u=t     indisprimary=t u=f
//!   ALTER … ADD PRIMARY KEY (a)  indisprimary=t u=t     indisprimary=t u=f
//!   PRIMARY KEY (a,b) inline     one row, u=t, n=2      two rows, both f
//!   UNIQUE (a,b) inline          one row, u=t, n=2      two rows, both f
//!   CREATE UNIQUE INDEX (a,b)    no pg_constraint row   contype='u'
//! ```
//!
//! `indisunique = false` on a primary key is a wrong VALUE, not a
//! spelling: anything reading it concludes the key is not unique, and
//! `pg_index` is where every PostgreSQL tool asks. It came from the
//! index's own flag — SPG records a primary key's uniqueness on the
//! table's constraint, not on the index — and `indisprimary` came from
//! testing whether the index NAME ends in `_pkey`, which made the two
//! spellings of one constraint disagree.
//!
//! The enforcement was never in question: both builds refuse the
//! duplicate and name the same columns. This is a tooling defect.
//!
//! Not closed here, and measured rather than assumed: an inline
//! composite constraint still produces TWO index rows where PostgreSQL
//! has one, and the index NAMES differ from PostgreSQL's
//! (`m2_a_pkey` against `m2_pkey`). Those follow from SPG building a
//! different set of indexes for the two spellings, which is a storage
//! question rather than a catalog one.

use spg_engine::{Engine, QueryResult};

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE m3 (a INT PRIMARY KEY)").unwrap();
    e.execute("CREATE TABLE m4 (a INT)").unwrap();
    e.execute("ALTER TABLE m4 ADD PRIMARY KEY (a)").unwrap();
    e.execute("CREATE TABLE m6 (a INT, b INT)").unwrap();
    e.execute("CREATE UNIQUE INDEX m6_ab ON m6 (a, b)").unwrap();
    e.execute("CREATE TABLE m7 (a INT, b INT)").unwrap();
    e.execute("CREATE INDEX m7_a ON m7 (a)").unwrap();
    e
}

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows");
    };
    rows.iter()
        .map(|r| {
            r.values
                .iter()
                .map(|v| match v {
                    spg_storage::Value::Text(t) => t.to_string(),
                    spg_storage::Value::Bool(b) => b.to_string(),
                    spg_storage::Value::Int(n) => n.to_string(),
                    spg_storage::Value::SmallInt(n) => n.to_string(),
                    spg_storage::Value::BigInt(n) => n.to_string(),
                    spg_storage::Value::Null => "NULL".to_string(),
                    o => panic!("{o:?}"),
                })
                .collect()
        })
        .collect()
}

/// `(indisprimary, indisunique)` for every index on `table`.
fn flags(e: &mut Engine, table: &str) -> Vec<(String, String)> {
    rows(
        e,
        &format!(
            "SELECT x.indisprimary, x.indisunique FROM pg_index x \
             JOIN pg_class c ON c.oid = x.indrelid WHERE c.relname = '{table}'"
        ),
    )
    .into_iter()
    .map(|r| (r[0].clone(), r[1].clone()))
    .collect()
}

#[test]
fn a_single_column_primary_key_is_unique() {
    // The shortest statement of the defect, and the commonest object in
    // any schema.
    let mut e = seeded();
    assert_eq!(flags(&mut e, "m3"), vec![("true".into(), "true".into())]);
}

#[test]
fn a_primary_key_added_by_alter_is_unique() {
    let mut e = seeded();
    assert_eq!(flags(&mut e, "m4"), vec![("true".into(), "true".into())]);
}

#[test]
fn a_bare_unique_index_is_unique_and_not_primary() {
    let mut e = seeded();
    assert_eq!(flags(&mut e, "m6"), vec![("false".into(), "true".into())]);
}

#[test]
fn a_plain_index_is_neither() {
    let mut e = seeded();
    assert_eq!(flags(&mut e, "m7"), vec![("false".into(), "false".into())]);
}

#[test]
fn a_bare_unique_index_creates_no_constraint() {
    // PostgreSQL creates none, so a schema-diff tool comparing the two
    // sides must not see one here.
    let mut e = seeded();
    let got = rows(
        &mut e,
        "SELECT conname FROM pg_constraint WHERE conname = 'm6_ab'",
    );
    assert!(got.is_empty(), "a phantom constraint: {got:?}");
}

#[test]
fn a_primary_key_still_has_its_constraint() {
    // The other direction: removing the phantom must not remove the
    // real ones.
    let mut e = seeded();
    // PostgreSQL 18.6 answers `n p` here — a NOT NULL constraint beside
    // the primary key — and so do we; the assertion was written for `p`
    // alone and had to be corrected against the oracle rather than the
    // other way round.
    let got = rows(
        &mut e,
        "SELECT contype FROM pg_constraint c JOIN pg_class t ON t.oid = c.conrelid \
         WHERE t.relname = 'm3' ORDER BY 1",
    );
    assert_eq!(
        got,
        vec![vec!["n".to_string()], vec!["p".to_string()]],
        "{got:?}"
    );
}

#[test]
fn the_enforcement_was_never_in_question() {
    // Said out loud because the severity of a catalog defect is easy to
    // over-read: the constraint itself always worked.
    let mut e = seeded();
    e.execute("INSERT INTO m3 VALUES (1)").unwrap();
    assert!(
        e.execute("INSERT INTO m3 VALUES (1)").is_err(),
        "the primary key must still refuse a duplicate"
    );
    e.execute("INSERT INTO m6 VALUES (1, 1)").unwrap();
    assert!(e.execute("INSERT INTO m6 VALUES (1, 1)").is_err());
}
