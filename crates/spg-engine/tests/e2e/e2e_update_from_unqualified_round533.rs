//! v7.39 (round 533) — an UNQUALIFIED source column in `UPDATE … FROM`.
//!
//! The last gap round 530 recorded. The parser lowers `UPDATE … FROM src
//! WHERE cond` onto correlated subqueries, and it can only classify a
//! QUALIFIED leaf: deciding whether an unqualified name belongs to the
//! target or to a source needs both column lists, which parse time does
//! not have.
//!
//!     UPDATE a SET v = v + d FROM b WHERE a.id = b.id
//!     PG18  110, 20      SPG  column "d" does not exist
//!
//! Measuring it turned up a second face nothing had reported. Where the
//! name is in BOTH relations, PG refuses the statement and SPG quietly
//! took the target's own value:
//!
//!     UPDATE a SET v = shared FROM b WHERE a.id = b.id
//!     PG18  ERROR: column reference "shared" is ambiguous
//!     SPG   wrote 7 — a's own `shared`, with nothing to say so
//!
//! The parser's lowering is untouched; it now records what it lowered
//! FROM, and the engine — which has the catalog — resolves the leaves
//! that lowering had to leave alone.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE a (id INT, v INT, shared INT)")
        .unwrap();
    e.execute("CREATE TABLE b (id INT, d INT, shared INT)")
        .unwrap();
    e.execute("INSERT INTO a VALUES (1,10,7),(2,20,7)").unwrap();
    e.execute("INSERT INTO b VALUES (1,100,9)").unwrap();
    e
}

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

/// A name that only the source has resolves there.
#[test]
fn round533_unqualified_source_column_resolves() {
    let mut e = engine();
    e.execute("UPDATE a SET v = v + d FROM b WHERE a.id = b.id")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT v FROM a ORDER BY id"),
        vec!["110", "20"]
    );
    // A constant assignment beside it, and a function call around it.
    e.execute("UPDATE a SET v = 0").unwrap();
    e.execute("UPDATE a SET v = abs(0 - d) FROM b WHERE a.id = b.id")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT v FROM a ORDER BY id"),
        vec!["100", "0"]
    );
}

/// A name both relations have is refused, with PG's wording — and the
/// table is left alone.
#[test]
fn round533_ambiguous_name_is_refused() {
    let mut e = engine();
    let err = e
        .execute("UPDATE a SET v = shared FROM b WHERE a.id = b.id")
        .expect_err("shared is in both");
    assert!(
        format!("{err}").contains("column reference \"shared\" is ambiguous"),
        "message was {err}"
    );
    assert_eq!(
        rows(&mut e, "SELECT v FROM a ORDER BY id"),
        vec!["10", "20"]
    );
}

/// A name only the target has still resolves to the target, and a name
/// neither has is still missing — the two readings that were already
/// right.
#[test]
fn round533_target_and_missing_names_unchanged() {
    let mut e = engine();
    e.execute("UPDATE a SET v = v + 1 FROM b WHERE a.id = b.id")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT v FROM a ORDER BY id"),
        vec!["11", "20"]
    );
    // With no FROM at all there is no source to resolve against.
    assert!(e.execute("UPDATE a SET v = d").is_err());
}

/// The qualified form the parser already handled is unchanged.
#[test]
fn round533_qualified_form_unchanged() {
    let mut e = engine();
    e.execute("UPDATE a SET v = b.d FROM b WHERE a.id = b.id")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT v FROM a ORDER BY id"),
        vec!["100", "20"]
    );
    // And a derived-table source, which round 530 fixed.
    e.execute("UPDATE a SET v = 0").unwrap();
    e.execute("UPDATE a SET v = v + x.d FROM (SELECT 1 AS id, 5 AS d) x WHERE a.id = x.id")
        .unwrap();
    assert_eq!(rows(&mut e, "SELECT v FROM a ORDER BY id"), vec!["5", "0"]);
}
