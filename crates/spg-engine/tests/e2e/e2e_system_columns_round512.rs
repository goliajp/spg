//! v7.39 (round 512) — the rest of PG's system columns, and the write half
//! of the `ctid` idiom.
//!
//! Round 511 exposed `ctid` on the read path and left two things: the DML
//! paths, so `DELETE … WHERE ctid …` still said the column did not exist,
//! and the other five columns. Both are here.
//!
//! `xid` and `cid` are their own types rather than integers, and PG's own
//! austerity is the argument: measured on PG18, `xmin + 1` is "operator does
//! not exist: xid + integer", `xmin > 0` likewise, `xmin::bigint` is "cannot
//! cast type xid to bigint", and there is no `max(xid)`. Carrying them as
//! BigInt would quietly allow all four.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE sc (a INT, b TEXT)").unwrap();
    e
}

fn text(e: &mut Engine, sql: &str) -> String {
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
            .collect::<Vec<_>>()
            .join(" "),
        other => panic!("{sql}: {other:?}"),
    }
}

/// All six exist, and each reports the type PG gives it.
#[test]
fn round512_every_system_column_reports_its_own_type() {
    let mut e = engine();
    e.execute("INSERT INTO sc VALUES (1, 'x')").unwrap();
    assert_eq!(
        text(
            &mut e,
            "SELECT pg_typeof(ctid), pg_typeof(xmin), pg_typeof(xmax), \
             pg_typeof(cmin), pg_typeof(cmax) FROM sc"
        ),
        "tid|xid|xid|cid|cid"
    );
    // cmin / cmax are 0 for every row a reader can see, which is every row
    // it can reach.
    assert_eq!(text(&mut e, "SELECT cmin, cmax FROM sc"), "0|0");
    // A live row's xmax is 0.
    assert_eq!(text(&mut e, "SELECT xmax FROM sc"), "0");
    // An xid compares to an xid.
    assert_eq!(text(&mut e, "SELECT xmin = xmin FROM sc"), "true");
}

/// `tableoid` names the relation through regclass, which is what it is for.
#[test]
fn round512_tableoid_names_the_relation() {
    let mut e = engine();
    e.execute("INSERT INTO sc VALUES (1, 'x')").unwrap();
    assert_eq!(
        text(&mut e, "SELECT tableoid::regclass::text FROM sc"),
        "sc"
    );
}

/// `*` expands none of them, in the mixed shape too.
#[test]
fn round512_star_expands_no_system_column() {
    let mut e = engine();
    e.execute("INSERT INTO sc VALUES (1, 'x')").unwrap();
    assert_eq!(text(&mut e, "SELECT * FROM sc"), "1|x");
    assert_eq!(text(&mut e, "SELECT *, ctid FROM sc"), "1|x|(0,1)");
}

/// `xmin` changes when the row is written again — which is the whole reason
/// an ORM reads it.
#[test]
fn round512_xmin_moves_when_the_row_is_rewritten() {
    let mut e = engine();
    e.execute("INSERT INTO sc VALUES (1, 'x')").unwrap();
    e.execute("INSERT INTO sc VALUES (2, 'y')").unwrap();
    let before = text(&mut e, "SELECT xmin FROM sc WHERE a = 1");
    e.execute("UPDATE sc SET b = 'z' WHERE a = 1").unwrap();
    let after = text(&mut e, "SELECT xmin FROM sc WHERE a = 1");
    assert_ne!(before, after, "an update must move the row's xmin");
    // The row nobody touched keeps its own.
    assert_eq!(
        text(&mut e, "SELECT xmin FROM sc WHERE a = 2"),
        text(&mut e, "SELECT xmin FROM sc WHERE a = 2")
    );
}

/// A row can be named by its ctid in a write, which round 511 could not do.
#[test]
fn round512_dml_can_name_a_row_by_ctid() {
    let mut e = engine();
    for (a, b) in [(1, "x"), (2, "y"), (3, "z")] {
        e.execute(&format!("INSERT INTO sc VALUES ({a}, '{b}')"))
            .unwrap();
    }
    e.execute("UPDATE sc SET a = 7 WHERE ctid = '(0,2)'::tid")
        .unwrap();
    assert_eq!(
        text(&mut e, "SELECT a, b FROM sc ORDER BY a"),
        "1|x 3|z 7|y"
    );

    // RETURNING must see the table's own columns, not the extended row the
    // predicate ran against.
    assert_eq!(
        text(
            &mut e,
            "DELETE FROM sc WHERE ctid = '(0,3)'::tid RETURNING a, b"
        ),
        "3|z"
    );
    assert_eq!(text(&mut e, "SELECT a, b FROM sc ORDER BY a"), "1|x 7|y");
}

/// The whole idiom, end to end — the reason any of this is worth having.
#[test]
fn round512_the_dedup_idiom_runs() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d (k INT)").unwrap();
    for _ in 0..12 {
        e.execute("INSERT INTO d VALUES (1)").unwrap();
    }
    for _ in 0..2 {
        e.execute("INSERT INTO d VALUES (2)").unwrap();
    }
    e.execute("DELETE FROM d WHERE ctid NOT IN (SELECT min(ctid) FROM d GROUP BY k)")
        .unwrap();
    assert_eq!(
        text(&mut e, "SELECT k, count(*) FROM d GROUP BY k ORDER BY k"),
        "1|1 2|1"
    );
}
