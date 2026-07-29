//! v7.39 (round 620) — an ungrouped column was reported as a column that does
//! not exist, and grouping by a primary key was refused outright.
//!
//! `SELECT id, count(*) FROM dc` answered `column "id" does not exist`. The
//! column plainly exists; what it is not is grouped. There was no rule at all
//! — the grouped row carries only the grouping keys and the aggregates, so the
//! reference simply failed to resolve at evaluation time and the resolver said
//! the only thing it knew. A user reading that goes hunting for a typo.
//!
//! Worse, and found by the same corpus file: `SELECT s, count(*) FROM dc GROUP
//! BY id` where `id` is the primary key is ANSWERED by PG and was REFUSED
//! here. One row per `id` means `s` has exactly one value in the group, so
//! there is nothing ambiguous — the SQL standard's functional dependency, which
//! PG applies for a base table's primary key. That is a query that runs on PG
//! and fails on SPG, which is worse than any wording.
//!
//! Letting the column past the check is not enough on its own: it still has
//! nowhere to be read from. The rewrite MySQL's loose GROUP BY already uses
//! (`col` → `any_value(col)`) is exactly right here and is reached for this
//! much narrower reason — under a primary key, "any value in the group" IS the
//! value.
//!
//! Two orderings had to be measured against PG rather than assumed:
//!
//!   * a GROUP BY name that resolves to nothing is reported as the missing
//!     column it is, AHEAD of this rule — PG answers `column "nosuch" does not
//!     exist` for `SELECT v FROM t GROUP BY nosuch`, not that `v` is ungrouped;
//!   * a select-list name that resolves to nothing is still a missing column —
//!     `SELECT nosuch, count(*) FROM t GROUP BY v` says so on both.
//!
//! The message also carries the right SQLSTATE now: an ungrouped column
//! reached the wire as 42703 UNDEFINED_COLUMN because that is what the engine
//! called it; it is 42803 GROUPING_ERROR.
//!
//! Scope is the single base table. Measured and NOT closed (checklist C06):
//! `SELECT p.s … FROM pk1 p JOIN pk1 q ON p.id=q.id GROUP BY p.id` is answered
//! by PG and refused here — the dependency would have to be traced across the
//! join. That error (`missing FROM-clause entry for table "p"`) predates this
//! round; the single-table spelling of it is what this round fixed.
//!
//! All 14 diagnostic shapes and 16 dependency shapes were checked against live
//! PG18.

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

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).expect_err(sql))
}

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE pk1 (id INT PRIMARY KEY, s TEXT, g INT)").unwrap();
    e.execute("CREATE TABLE pk2 (a INT, b INT, s TEXT, PRIMARY KEY (a,b))").unwrap();
    e.execute("CREATE TABLE uq1 (id INT UNIQUE, s TEXT)").unwrap();
    e.execute("INSERT INTO pk1 VALUES (1,'x',10),(2,'y',20),(3,'z',10)").unwrap();
    e.execute("INSERT INTO pk2 VALUES (1,1,'p'),(1,2,'q'),(2,1,'r')").unwrap();
    e.execute("INSERT INTO uq1 VALUES (1,'x'),(2,'y')").unwrap();
    e
}

/// The diagnosis: what it says, and who it names.
#[test]
fn round620_an_ungrouped_column_says_so() {
    let mut e = seed();
    for sql in [
        "SELECT s, count(*) FROM pk1",
        "SELECT s, count(*) FROM pk1 HAVING count(*) > 0",
        "SELECT s, count(*) FROM pk1 GROUP BY g",
        "SELECT s || 'x', count(*) FROM pk1 GROUP BY g",
        "SELECT upper(s), count(*) FROM pk1 GROUP BY g",
        "SELECT s FROM pk1 GROUP BY g",
        "SELECT count(*) FROM pk1 HAVING s > ''",
        "SELECT s, count(*) FROM pk1 ORDER BY 1",
        "SELECT CASE WHEN g > 0 THEN s END, count(*) FROM pk1 GROUP BY g",
    ] {
        assert!(
            err(&mut e, sql)
                .contains(r#"column "pk1.s" must appear in the GROUP BY clause or be used in an aggregate function"#),
            "{sql} said: {}",
            err(&mut e, sql)
        );
    }
    assert!(
        err(&mut e, "SELECT s, count(*) FROM pk1 p GROUP BY p.g")
            .contains(r#"column "p.s""#),
        "the alias is what PG qualifies it with when there is one"
    );
}

/// Two orderings, both measured against PG rather than assumed.
#[test]
fn round620_a_name_that_resolves_to_nothing_is_still_missing() {
    let mut e = seed();
    let m = err(&mut e, "SELECT s FROM pk1 GROUP BY nosuch");
    assert!(
        m.contains("nosuch") && !m.contains("must appear in the GROUP BY"),
        "an unresolvable GROUP BY name is reported first: {m}"
    );
    let m = err(&mut e, "SELECT nosuch, count(*) FROM pk1 GROUP BY g");
    assert!(
        m.contains("nosuch") && !m.contains("must appear in the GROUP BY"),
        "and a select-list name that names nothing is still missing: {m}"
    );
}

/// The functional dependency: grouping by a primary key licenses the rest of
/// that table's columns.
#[test]
fn round620_grouping_by_a_primary_key() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT s, count(*) FROM pk1 GROUP BY id ORDER BY 1"),
        vec!["x|1", "y|1", "z|1"]
    );
    assert_eq!(
        vals(&mut e, "SELECT id, s, g, count(*) FROM pk1 GROUP BY id ORDER BY 1"),
        vec!["1|x|10|1", "2|y|20|1", "3|z|10|1"],
        "every other column, not just one"
    );
    assert_eq!(
        vals(&mut e, "SELECT upper(s) || g::TEXT, count(*) FROM pk1 GROUP BY id ORDER BY 1"),
        vec!["X10|1", "Y20|1", "Z10|1"],
        "inside an expression"
    );
    assert_eq!(
        vals(&mut e, "SELECT CASE WHEN g > 15 THEN s ELSE 'lo' END, count(*) FROM pk1 GROUP BY id ORDER BY 1"),
        vec!["lo|1", "lo|1", "y|1"]
    );
    assert_eq!(
        vals(&mut e, "SELECT s, count(*) FROM pk1 GROUP BY pk1.id ORDER BY 1"),
        vec!["x|1", "y|1", "z|1"],
        "the key grouped by its qualified spelling"
    );
    assert_eq!(
        vals(&mut e, "SELECT s, count(*) FROM pk1 p GROUP BY p.id ORDER BY 1"),
        vec!["x|1", "y|1", "z|1"],
        "and under an alias"
    );
    assert_eq!(
        vals(&mut e, "SELECT s, count(*) FROM pk1 GROUP BY id HAVING count(*) = 1 ORDER BY 1"),
        vec!["x|1", "y|1", "z|1"],
        "with a HAVING, which the check also walks"
    );
    assert_eq!(
        vals(&mut e, "SELECT (SELECT count(*) FROM uq1), count(*) FROM pk1 GROUP BY id ORDER BY 1"),
        vec!["2|1", "2|1", "2|1"],
        "an uncorrelated subquery in the select list is nobody's business here"
    );
}

/// A composite primary key needs ALL of its columns, in any order.
#[test]
fn round620_composite_primary_key() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT s, count(*) FROM pk2 GROUP BY a, b ORDER BY 1"),
        vec!["p|1", "q|1", "r|1"]
    );
    assert_eq!(
        vals(&mut e, "SELECT s, count(*) FROM pk2 GROUP BY b, a ORDER BY 1"),
        vec!["p|1", "q|1", "r|1"],
        "order of the grouping list is irrelevant"
    );
    assert!(
        err(&mut e, "SELECT s, count(*) FROM pk2 GROUP BY a")
            .contains(r#"column "pk2.s" must appear"#),
        "HALF the key determines nothing"
    );
}

/// What must NOT be mistaken for the dependency.
#[test]
fn round620_what_does_not_license_it() {
    let mut e = seed();
    assert!(
        err(&mut e, "SELECT s, count(*) FROM uq1 GROUP BY id")
            .contains(r#"column "uq1.s" must appear"#),
        "UNIQUE is not PRIMARY KEY — a NULL key can repeat, so it determines nothing"
    );
    assert!(
        err(&mut e, "SELECT s, count(*) FROM pk1 GROUP BY id + 0")
            .contains(r#"column "pk1.s" must appear"#),
        "the key has to be grouped by AS ITSELF; an expression over it is not the key"
    );
    assert!(
        err(&mut e, "SELECT s, count(*) FROM pk1 GROUP BY g")
            .contains(r#"column "pk1.s" must appear"#),
        "a non-key column determines nothing"
    );
}
