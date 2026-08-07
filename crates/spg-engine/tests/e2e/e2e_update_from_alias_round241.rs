//! v7.39 (round 241) — UPDATE…FROM / DELETE…USING sweep against live
//! PG18.4 (2026-07-19). The joined-DML core (multi-table FROM, subquery
//! sources, self-joins, EXCLUDED-free assignment rewriting) already
//! matched; the gaps:
//!
//!   * `UPDATE emp e SET …` / `DELETE FROM emp d USING …` — a bare
//!     target alias did not parse (PG allows the bare spelling on both,
//!     unlike INSERT which requires AS). The alias is the statement's
//!     table qualifier: assignments, WHERE and RETURNING all read the
//!     target row through it.
//!   * RETURNING could not reference the FROM / USING tables
//!     (`RETURNING emp.id, dept.name` died with "unknown table
//!     qualifier"); those leaves now get the same correlated-scalar-
//!     subquery lowering the assignments always got.
//!   * An unknown qualifier now reports PG's "missing FROM-clause entry
//!     for table \"x\"" (42P01) instead of SPG's own wording.
//!
//! Deliberate divergence, already documented in the lowering: a
//! multi-match join (no usable predicate) raises the scalar-subquery
//! cardinality error where PG silently picks an arbitrary row.

use spg_engine::{Engine, QueryResult};

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE emp (id int PRIMARY KEY, dept int, sal int)")
        .unwrap();
    e.execute("CREATE TABLE dept (id int PRIMARY KEY, bonus int, name text)")
        .unwrap();
    e.execute("INSERT INTO emp VALUES (1,10,100),(2,10,200),(3,20,300),(4,30,400)")
        .unwrap();
    e.execute("INSERT INTO dept VALUES (10,5,'a'),(20,7,'b')")
        .unwrap();
    e
}

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
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

#[test]
fn bare_target_alias_parses_and_binds() {
    let mut e = seeded();
    // UPDATE with a bare alias; the alias qualifies both sides.
    e.execute("UPDATE emp e SET sal = e.sal + d.bonus FROM dept d WHERE e.dept = d.id")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT id, sal FROM emp ORDER BY id"),
        ["1|105", "2|205", "3|307", "4|400"]
    );
    // The AS spelling too.
    e.execute("UPDATE emp AS z SET sal = 1 WHERE z.id = 4")
        .unwrap();
    assert_eq!(rows(&mut e, "SELECT sal FROM emp WHERE id = 4"), ["1"]);
    // DELETE with a bare alias.
    e.execute("DELETE FROM emp d USING dept WHERE d.dept = dept.id AND dept.name = 'b'")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT id FROM emp ORDER BY id"),
        ["1", "2", "4"]
    );
}

#[test]
fn returning_reaches_the_from_and_using_tables() {
    let mut e = seeded();
    // UPDATE…FROM RETURNING a FROM-table column.
    assert_eq!(
        rows(
            &mut e,
            "UPDATE emp SET sal = sal + 1 FROM dept WHERE emp.dept = dept.id \
             RETURNING emp.id, dept.name"
        ),
        ["1|a", "2|a", "3|b"]
    );
    // DELETE…USING RETURNING both the target (by alias) and USING table.
    assert_eq!(
        rows(
            &mut e,
            "DELETE FROM emp d USING dept WHERE d.dept = dept.id AND dept.name = 'b' \
             RETURNING d.id, dept.name"
        ),
        ["3|b"]
    );
}

#[test]
fn unknown_qualifier_takes_pgs_wording() {
    let mut e = seeded();
    let got = format!(
        "{}",
        e.execute("UPDATE emp SET sal = missing.x WHERE id = 1")
            .unwrap_err()
    );
    assert!(
        got.contains("missing FROM-clause entry for table \"missing\""),
        "{got}"
    );
}

#[test]
fn the_joined_dml_core_is_unchanged() {
    let mut e = seeded();
    // Regression guard over the sweep's clean cases.
    e.execute("UPDATE emp SET sal = 0 FROM dept WHERE emp.dept = dept.id AND dept.name = 'a'")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT id, sal FROM emp ORDER BY id"),
        ["1|0", "2|0", "3|300", "4|400"]
    );
    e.execute(
        "UPDATE emp SET sal = 1 FROM (SELECT id FROM dept WHERE bonus > 6) s WHERE emp.dept = s.id",
    )
    .unwrap();
    assert_eq!(rows(&mut e, "SELECT sal FROM emp WHERE id = 3"), ["1"]);
    e.execute("DELETE FROM emp USING dept WHERE emp.dept = dept.id AND dept.name = 'b'")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT id FROM emp ORDER BY id"),
        ["1", "2", "4"]
    );
}
