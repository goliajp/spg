//! v7.39 (round 621) — in a joined aggregate query, every qualified column
//! that was not a grouping key came back `missing FROM-clause entry for table
//! "a"`.
//!
//! `a` is right there in the FROM clause. The message is what round 620 fixed
//! the single-table spelling of, and the join spelling of it was worse,
//! because it was not only a bad diagnosis — it refused seven shapes below
//! that PG answers, and gave the wrong reason for the ones PG also refuses.
//!
//! Two causes, one line apart:
//!
//!   * a joined schema names its columns `a.s`, and round 620's walker matched
//!     only the BARE name. So a qualified reference was invisible to it: not
//!     reported as ungrouped, not rewritten, and left to fail at evaluation
//!     time against a grouped row that carries neither;
//!   * the functional dependency was a single boolean for the whole query, and
//!     the whole query is the wrong unit. A join has one primary key per side.
//!
//! The dependency is now the SET of qualifiers whose key is wholly grouped, so
//! a join licenses the side whose key is grouped and refuses the other —
//! `SELECT a.s, b.t … GROUP BY a.id` answers `a.s` and still refuses `b.t`,
//! which is what PG does. A key column counts only when it is grouped as
//! itself and as THAT table's: an unqualified spelling of it only counts when
//! there is a single table for it to mean.
//!
//! Nine shapes and ten boundary shapes were checked against live PG18, all
//! byte-identical — including the ones that must still be refused, which now
//! say what is actually wrong with them. One expectation in a first cut of
//! this file was written from assumption rather than measurement (a self-join
//! grouped by the other alias's key) and was wrong in the permissive
//! direction; PG refuses it, because the dependency is not traced through the
//! join predicate. It is pinned below as the refusal it is.

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
    e.execute("CREATE TABLE na (id INT PRIMARY KEY, s TEXT, g INT)")
        .unwrap();
    // nb deliberately has NO primary key, so it is the unlicensed side.
    e.execute("CREATE TABLE nb (bid INT, t TEXT, aid INT)")
        .unwrap();
    e.execute("CREATE TABLE uu (id INT UNIQUE, s TEXT)")
        .unwrap();
    e.execute("INSERT INTO na VALUES (1,'x',10),(2,'y',20),(3,'z',10)")
        .unwrap();
    e.execute("INSERT INTO nb VALUES (10,'p',1),(11,'q',1),(12,'r',2)")
        .unwrap();
    e.execute("INSERT INTO uu VALUES (1,'x'),(2,'y')").unwrap();
    e
}

/// The shapes PG answers and this refused.
#[test]
fn round621_a_grouped_key_licenses_its_own_side() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.s, count(*) FROM na a JOIN nb b ON b.aid=a.id GROUP BY a.id ORDER BY 1"
        ),
        vec!["x|2", "y|1"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.s, a.g, count(*) FROM na a JOIN nb b ON b.aid=a.id GROUP BY a.id ORDER BY 1"
        ),
        vec!["x|10|2", "y|20|1"],
        "every other column of the licensed side, not just one"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.s || b.t, count(*) FROM na a JOIN nb b ON b.aid=a.id GROUP BY a.id, b.t ORDER BY 1"
        ),
        vec!["xp|1", "xq|1", "yr|1"],
        "inside an expression, beside a grouped column of the other side"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.s, count(*) FROM na a, nb b WHERE b.aid=a.id GROUP BY a.id ORDER BY 1"
        ),
        vec!["x|2", "y|1"],
        "the comma spelling of the join"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.s, count(*) FROM na a LEFT JOIN nb b ON b.aid=a.id GROUP BY a.id ORDER BY 1"
        ),
        vec!["x|2", "y|1", "z|1"],
        "and an outer join, where the unmatched row still forms its own group"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.s, count(*) FROM na a JOIN nb b ON b.aid=a.id GROUP BY a.id HAVING max(b.t) > '' ORDER BY 1"
        ),
        vec!["x|2", "y|1"],
        "with a HAVING over the other side"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.s, count(*) FROM na a JOIN nb b ON b.aid=a.id GROUP BY a.id ORDER BY a.g"
        ),
        vec!["x|2", "y|1"],
        "ordering by a licensed column that is not projected"
    );
}

/// A self-join where the OTHER alias carries the grouped key. Measured: PG
/// refuses it, and so does this — the join condition says `a.id = c.id`, but
/// the dependency is not traced THROUGH a predicate, only from a table's own
/// grouped key to its own columns. Asserting it because a first cut of the pin
/// assumed the opposite and the assumption was wrong.
#[test]
fn round621_a_key_does_not_travel_along_the_join_condition() {
    let mut e = seed();
    assert!(
        err(
            &mut e,
            "SELECT a.s, count(*) FROM na a JOIN na c ON a.id=c.id GROUP BY c.id"
        )
        .contains(r#"column "a.s" must appear in the GROUP BY clause"#)
    );
}

/// One key licenses one side. The other side is still the other side.
#[test]
fn round621_the_other_side_stays_refused() {
    let mut e = seed();
    assert!(
        err(
            &mut e,
            "SELECT b.t, count(*) FROM na a JOIN nb b ON b.aid=a.id GROUP BY a.id"
        )
        .contains(r#"column "b.t" must appear in the GROUP BY clause"#),
        "nb has no primary key, so nothing determines its columns: {}",
        err(
            &mut e,
            "SELECT b.t, count(*) FROM na a JOIN nb b ON b.aid=a.id GROUP BY a.id"
        )
    );
    assert!(
        err(
            &mut e,
            "SELECT a.s, b.t, count(*) FROM na a JOIN nb b ON b.aid=a.id GROUP BY a.id"
        )
        .contains(r#"column "b.t" must appear in the GROUP BY clause"#),
        "the licensed column beside the unlicensed one — the unlicensed one decides"
    );
    assert!(
        err(
            &mut e,
            "SELECT a.s, count(*) FROM na a JOIN nb b ON b.aid=a.id GROUP BY a.g"
        )
        .contains(r#"column "a.s" must appear in the GROUP BY clause"#),
        "grouping by a NON-key column of the licensed side licenses nothing"
    );
    assert!(
        err(
            &mut e,
            "SELECT a.s, count(*) FROM na a JOIN nb b ON b.aid=a.id GROUP BY a.id ORDER BY b.t"
        )
        .contains(r#"column "b.t" must appear in the GROUP BY clause"#),
        "ORDER BY is walked too"
    );
    assert!(
        err(
            &mut e,
            "SELECT u.s, count(*) FROM na a JOIN uu u ON u.id=a.id GROUP BY a.id"
        )
        .contains(r#"column "u.s" must appear in the GROUP BY clause"#),
        "UNIQUE is not PRIMARY KEY — a NULL key can repeat, so it determines nothing"
    );
    assert!(
        err(
            &mut e,
            "SELECT a.s, count(*) FROM na a JOIN uu u ON u.id=a.id GROUP BY u.id"
        )
        .contains(r#"column "a.s" must appear in the GROUP BY clause"#),
        "and grouping by the UNIQUE side licenses neither side"
    );
}

/// The single-table case round 620 closed must not have moved.
#[test]
fn round621_single_table_is_unchanged() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT s, count(*) FROM na GROUP BY id ORDER BY 1"),
        vec!["x|1", "y|1", "z|1"],
        "unqualified, which only counts because there is one table to mean"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT s, count(*) FROM na a GROUP BY a.id ORDER BY 1"
        ),
        vec!["x|1", "y|1", "z|1"]
    );
    assert!(
        err(&mut e, "SELECT s, count(*) FROM na GROUP BY g")
            .contains(r#"column "na.s" must appear in the GROUP BY clause"#)
    );
}
