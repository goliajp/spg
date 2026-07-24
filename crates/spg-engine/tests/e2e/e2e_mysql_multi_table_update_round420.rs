//! read01 round 420 (MySQL differential) — multi-table UPDATE.
//!
//! MySQL's join-update idiom names its sources up front instead of in a FROM
//! clause, and SPG rejected every spelling at parse time:
//!     UPDATE a, b SET a.v = b.v WHERE a.id = b.id           -> error at ","
//!     UPDATE a JOIN b ON a.id = b.id SET a.v = b.v + 1      -> error
//!     UPDATE a LEFT JOIN b ON a.id = b.id SET a.v = …       -> error at "LEFT"
//!
//! Shape-wise this is exactly PG's `UPDATE a SET … FROM b WHERE …`, which SPG
//! already lowers onto correlated subqueries — so the first table becomes the
//! mutation target, the rest become the source FROM list, and a `JOIN … ON`
//! predicate folds into the WHERE. A LEFT join skips the EXISTS row filter so
//! every target row still updates (reading NULL from the unmatched source).
//!
//! Multi-TARGET updates (`SET a.v = 1, b.v = 2`, mutating two tables in one
//! statement) are NOT modelled and are refused loudly — see the dedicated
//! test. That matters: `expect_ident_like` silently strips a `<qual>.`
//! prefix, so without the check `SET b.v = 888` would have written 888 into
//! `a.v` while naming `b`.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn col_v(e: &mut Engine, sql: &str) -> Vec<i64> {
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

/// The comma form: matched rows take the source value, unmatched keep theirs.
#[test]
fn comma_join_form() {
    let mut e = seed();
    e.execute("UPDATE a, b SET a.v = b.v WHERE a.id = b.id").unwrap();
    assert_eq!(col_v(&mut e, "SELECT v FROM a ORDER BY id"), vec![100, 200, 30]);
}

/// The explicit JOIN form, with the predicate in ON.
#[test]
fn inner_join_form() {
    let mut e = seed();
    e.execute("UPDATE a JOIN b ON a.id = b.id SET a.v = b.v + 1")
        .unwrap();
    assert_eq!(col_v(&mut e, "SELECT v FROM a ORDER BY id"), vec![101, 201, 30]);
}

/// LEFT JOIN updates EVERY target row — an unmatched one reads NULL from the
/// source side (here COALESCE turns it into -1).
#[test]
fn left_join_form_updates_unmatched_rows() {
    let mut e = seed();
    e.execute("UPDATE a LEFT JOIN b ON a.id = b.id SET a.v = COALESCE(b.v, -1)")
        .unwrap();
    assert_eq!(col_v(&mut e, "SELECT v FROM a ORDER BY id"), vec![100, 200, -1]);
}

/// Aliases on both sides.
#[test]
fn alias_form() {
    let mut e = seed();
    e.execute("UPDATE a AS x, b AS y SET x.v = y.v WHERE x.id = y.id")
        .unwrap();
    assert_eq!(col_v(&mut e, "SELECT v FROM a ORDER BY id"), vec![100, 200, 30]);
}

/// An unqualified assignment target still resolves to the update target.
#[test]
fn unqualified_assignment_target() {
    let mut e = mysql();
    e.execute("CREATE TABLE a(id INT, v INT)").unwrap();
    e.execute("CREATE TABLE b(id INT, w INT)").unwrap();
    e.execute("INSERT INTO a VALUES(1,10),(2,20)").unwrap();
    e.execute("INSERT INTO b VALUES(1,100)").unwrap();
    e.execute("UPDATE a, b SET v = b.w WHERE a.id = b.id").unwrap();
    assert_eq!(col_v(&mut e, "SELECT v FROM a ORDER BY id"), vec![100, 20]);
}

/// A multi-TARGET update is refused, and NEITHER table is touched — the
/// alternative (silently writing the source-qualified value into the target)
/// is the silent-wrong this check exists to prevent.
#[test]
fn multi_target_is_refused_without_mutating() {
    let mut e = seed();
    assert!(
        e.execute("UPDATE a, b SET a.v = 777, b.v = 888 WHERE a.id = b.id")
            .is_err(),
        "multi-target UPDATE must be refused"
    );
    assert!(
        e.execute("UPDATE a, b SET b.v = 5 WHERE a.id = b.id").is_err(),
        "assigning only to a source table must be refused"
    );
    // Both tables untouched.
    assert_eq!(col_v(&mut e, "SELECT v FROM a ORDER BY id"), vec![10, 20, 30]);
    assert_eq!(col_v(&mut e, "SELECT v FROM b ORDER BY id"), vec![100, 200]);
}

/// A PostgreSQL session has no multi-table UPDATE and still rejects the comma
/// form; its own `UPDATE … FROM …` is unchanged.
#[test]
fn postgres_unchanged() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE a(id INT, v INT)").unwrap();
    e.execute("CREATE TABLE b(id INT, w INT)").unwrap();
    e.execute("INSERT INTO a VALUES(1,10),(2,20)").unwrap();
    e.execute("INSERT INTO b VALUES(1,100)").unwrap();
    assert!(
        e.execute("UPDATE a, b SET a.v = b.w WHERE a.id = b.id").is_err(),
        "PG has no multi-table UPDATE"
    );
    e.execute("UPDATE a SET v = b.w FROM b WHERE a.id = b.id")
        .unwrap();
    assert_eq!(col_v(&mut e, "SELECT v FROM a ORDER BY id"), vec![100, 20]);
}
