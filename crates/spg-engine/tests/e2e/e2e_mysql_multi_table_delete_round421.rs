//! read01 round 421 (MySQL differential) — multi-table DELETE, plus the
//! LEFT-JOIN `WHERE` correction to round 420's multi-table UPDATE.
//!
//! MySQL names the DELETE target before the FROM and joins its sources
//! there; SPG rejected every spelling:
//!     DELETE a FROM a JOIN b ON a.id = b.id
//!     DELETE a FROM a, b WHERE a.id = b.id
//!     DELETE FROM a USING a, b WHERE a.id = b.id   (target repeated)
//!     DELETE a FROM a LEFT JOIN b ON … WHERE b.id IS NULL   (anti-join)
//!
//! All of them lower onto the correlated-subquery machinery PG's
//! `DELETE FROM t USING src WHERE …` already uses. Multi-TARGET deletes
//! (`DELETE a, b FROM …`) are not modelled and are refused.
//!
//! ROUND-420 CORRECTION: a LEFT join must filter the SOURCE subquery on the
//! ON predicate ALONE — the WHERE still selects TARGET rows. Round 420
//! folded ON into WHERE and dropped the outer filter, so
//! `UPDATE a LEFT JOIN b ON … SET … WHERE a.id > 1` updated EVERY row. The
//! same split now drives DELETE, which is what makes the anti-join idiom
//! (`LEFT JOIN … WHERE b.id IS NULL`) come out right.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn ints(e: &mut Engine, sql: &str) -> Vec<i64> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                Value::Int(n) => i64::from(*n),
                Value::BigInt(n) => *n,
                o => panic!("{o:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn seed() -> Engine {
    let mut e = mysql();
    e.execute("CREATE TABLE a(id INT, v INT)").unwrap();
    e.execute("CREATE TABLE b(id INT, v INT)").unwrap();
    e.execute("INSERT INTO a VALUES(1,10),(2,20),(3,30)").unwrap();
    e.execute("INSERT INTO b VALUES(1,100),(2,200)").unwrap();
    e
}

/// `DELETE a FROM a JOIN b ON …` deletes the matched target rows and leaves
/// the source table alone.
#[test]
fn join_form_deletes_matched_target_rows_only() {
    let mut e = seed();
    e.execute("DELETE a FROM a JOIN b ON a.id = b.id").unwrap();
    assert_eq!(ints(&mut e, "SELECT id FROM a ORDER BY id"), vec![3]);
    assert_eq!(ints(&mut e, "SELECT COUNT(*) FROM b"), vec![2]);
}

/// The comma form.
#[test]
fn comma_form() {
    let mut e = seed();
    e.execute("DELETE a FROM a, b WHERE a.id = b.id").unwrap();
    assert_eq!(ints(&mut e, "SELECT id FROM a ORDER BY id"), vec![3]);
}

/// MySQL's `USING` spelling repeats the TARGET as the first source; it must
/// be peeled so the subquery does not re-scan (and shadow) the target.
#[test]
fn using_form_with_target_repeated() {
    let mut e = seed();
    e.execute("DELETE FROM a USING a, b WHERE a.id = b.id").unwrap();
    assert_eq!(ints(&mut e, "SELECT id FROM a ORDER BY id"), vec![3]);
}

/// Aliases on both sides, with the pre-FROM target naming the alias.
#[test]
fn alias_form() {
    let mut e = seed();
    e.execute("DELETE x FROM a AS x JOIN b AS y ON x.id = y.id")
        .unwrap();
    assert_eq!(ints(&mut e, "SELECT id FROM a ORDER BY id"), vec![3]);
}

/// The anti-join idiom: LEFT JOIN + `IS NULL` deletes the rows with NO match.
/// This is the shape the ON / WHERE split exists for.
#[test]
fn left_join_anti_join_deletes_unmatched() {
    let mut e = seed();
    e.execute("DELETE a FROM a LEFT JOIN b ON a.id = b.id WHERE b.id IS NULL")
        .unwrap();
    assert_eq!(ints(&mut e, "SELECT id FROM a ORDER BY id"), vec![1, 2]);
}

/// Multi-TARGET and wrong-target deletes are refused, touching nothing.
#[test]
fn refused_forms_mutate_nothing() {
    let mut e = seed();
    assert!(
        e.execute("DELETE a, b FROM a JOIN b ON a.id = b.id").is_err(),
        "multi-target DELETE must be refused"
    );
    assert!(
        e.execute("DELETE b FROM a JOIN b ON a.id = b.id").is_err(),
        "a target that is not the FROM primary must be refused"
    );
    assert_eq!(ints(&mut e, "SELECT COUNT(*) FROM a"), vec![3]);
    assert_eq!(ints(&mut e, "SELECT COUNT(*) FROM b"), vec![2]);
}

/// ROUND-420 CORRECTION — a LEFT-joined UPDATE still honours its WHERE.
#[test]
fn update_left_join_honours_where() {
    let mut e = seed();
    e.execute("UPDATE a LEFT JOIN b ON a.id = b.id SET a.v = 999 WHERE a.id > 1")
        .unwrap();
    // Row 1 is excluded by the WHERE; rows 2 and 3 (3 unmatched) update.
    assert_eq!(ints(&mut e, "SELECT v FROM a ORDER BY id"), vec![10, 999, 999]);
}

/// The round-420 UPDATE forms still behave.
#[test]
fn update_forms_unchanged() {
    let mut e = seed();
    e.execute("UPDATE a, b SET a.v = b.v WHERE a.id = b.id").unwrap();
    assert_eq!(ints(&mut e, "SELECT v FROM a ORDER BY id"), vec![100, 200, 30]);
    let mut e2 = seed();
    e2.execute("UPDATE a LEFT JOIN b ON a.id = b.id SET a.v = COALESCE(b.v, -1)")
        .unwrap();
    assert_eq!(ints(&mut e2, "SELECT v FROM a ORDER BY id"), vec![100, 200, -1]);
}

/// A PostgreSQL session rejects the MySQL spelling; its own `USING` form is
/// unchanged.
#[test]
fn postgres_unchanged() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE a(id INT, v INT)").unwrap();
    e.execute("CREATE TABLE b(id INT, w INT)").unwrap();
    e.execute("INSERT INTO a VALUES(1,10),(2,20),(3,30)").unwrap();
    e.execute("INSERT INTO b VALUES(1,100),(2,200)").unwrap();
    assert!(
        e.execute("DELETE a FROM a JOIN b ON a.id = b.id").is_err(),
        "PG has no pre-FROM DELETE target"
    );
    e.execute("DELETE FROM a USING b WHERE a.id = b.id").unwrap();
    assert_eq!(ints(&mut e, "SELECT id FROM a ORDER BY id"), vec![3]);
}
