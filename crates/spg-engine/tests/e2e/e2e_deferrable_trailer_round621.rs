//! v7.39 (round 621) — every DEFERRABLE spelling on a PK / UNIQUE was a parse
//! error.
//!
//! `CREATE TABLE t (id INT PRIMARY KEY DEFERRABLE INITIALLY DEFERRED)` is what
//! pg_dump writes for a table declared that way, and SPG answered `syntax
//! error at or near "DEFERRABLE"` — so the restore stopped there. The FK path
//! has consumed these clauses since round 288; the inline and table-level
//! PK / UNIQUE arms never learned them.
//!
//! The clauses are consumed and recorded nowhere: SPG enforces the constraint
//! IMMEDIATELY either way. Inside a transaction that violates-then-repairs
//! before COMMIT, PG (deferred) succeeds where SPG fails at the violating
//! statement — a REFUSAL, not a wrong answer, and the honest half of the
//! feature. True deferral is the open remainder of F08.
//!
//! Six of the seven probe shapes match live PG18 byte for byte. The seventh is
//! recorded as an accepted divergence: PG refuses an FK that references a
//! deferrable unique constraint (`cannot use a deferrable unique constraint
//! for referenced table`), SPG accepts it — having dropped the deferrability,
//! the referenced constraint IS immediate here, so the FK is sound.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
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

/// Every spelling, in both positions.
#[test]
fn round621_the_spellings_parse() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d1 (id INT PRIMARY KEY DEFERRABLE INITIALLY DEFERRED)").unwrap();
    e.execute("CREATE TABLE d2 (id INT PRIMARY KEY DEFERRABLE)").unwrap();
    e.execute("CREATE TABLE d3 (id INT UNIQUE DEFERRABLE INITIALLY IMMEDIATE)").unwrap();
    e.execute("CREATE TABLE d4 (id INT PRIMARY KEY NOT DEFERRABLE)").unwrap();
    e.execute("CREATE TABLE d5 (id INT UNIQUE INITIALLY DEFERRED)")
        .expect("bare INITIALLY implies DEFERRABLE, as on the FK path");
    e.execute(
        "CREATE TABLE d6 (a INT, b INT, UNIQUE (a, b) DEFERRABLE INITIALLY DEFERRED, \
         PRIMARY KEY (a) NOT DEFERRABLE)",
    )
    .expect("table-level constraints take the trailer too");
}

/// What the constraint still does — the deferral is dropped, not the
/// constraint.
#[test]
fn round621_enforcement_is_immediate() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d1 (id INT PRIMARY KEY DEFERRABLE INITIALLY DEFERRED)").unwrap();
    e.execute("INSERT INTO d1 VALUES (1),(2)").unwrap();
    assert_eq!(vals(&mut e, "SELECT count(*) FROM d1"), vec!["2"]);
    assert!(
        e.execute("INSERT INTO d1 VALUES (1)").is_err(),
        "the PK is real; only its timing was dropped"
    );
    let mut e2 = Engine::new();
    e2.execute("CREATE TABLE u1 (a INT, b INT, UNIQUE (a, b) DEFERRABLE)").unwrap();
    e2.execute("INSERT INTO u1 VALUES (1, 2)").unwrap();
    assert!(
        e2.execute("INSERT INTO u1 VALUES (1, 2)").is_err(),
        "and so is the table-level UNIQUE"
    );
}

/// The neighbours that must not have moved.
#[test]
fn round621_the_neighbouring_clauses() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE n1 (id INT PRIMARY KEY, s TEXT NOT NULL)").unwrap();
    assert!(
        e.execute("INSERT INTO n1 (id) VALUES (1)").is_err(),
        "NOT NULL — whose leading NOT the new arm must not eat"
    );
    e.execute("CREATE TABLE n2 (id INT UNIQUE NULLS NOT DISTINCT DEFERRABLE)").unwrap();
    e.execute("INSERT INTO n2 VALUES (NULL)").unwrap();
    assert!(
        e.execute("INSERT INTO n2 VALUES (NULL)").is_err(),
        "NULLS NOT DISTINCT still parses in front of the trailer, and still binds"
    );
}
