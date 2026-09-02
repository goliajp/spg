//! v7.39.11 — the ordered-index walk's gate answers for the executor
//! that runs, and refuses an ordering the tree does not hold.
//!
//! Two defects, found while giving the walk its LIMIT.
//!
//! **The plan misnamed the access path.** There are two index-ordered
//! walks — the streaming one, whose gate `EXPLAIN` asks, and the
//! materialising top-N one — and only the first knew that a composite
//! B-tree LEADING on the ORDER BY column can be walked. On a table
//! indexed `(a, b)`, `SELECT a FROM m ORDER BY a LIMIT 2` planned as
//! `Limit -> Sort -> Seq Scan` while the executor plainly walked the
//! index. r1044 exists to keep those two answers together and says
//! why: EXPLAIN is the first thing any performance question opens, and
//! an instrument that misnames the access path is worse than one that
//! says nothing.
//!
//! **And the walk took orderings the tree does not hold.** A B-tree
//! walks in BYTE order unless the column's keys are ICU sort keys, and
//! this gate never asked — so on a MySQL-dialect session the answer
//! changed when an index appeared:
//!
//! ```text
//!   ORDER BY t over alpha / Beta / GAMMA / delta
//!     no index   alpha Beta delta GAMMA   (MySQL's own order)
//!     indexed    Beta GAMMA alpha delta   (bytes)
//! ```
//!
//! No row is wrong and nothing raises; only the order changes, and it
//! changes because an index exists. `try_pk_walk_top_n` has asked this
//! since v7.38.18.

use spg_engine::{CancelToken, Engine, QueryResult, StreamItem};
use spg_storage::Value;

fn plan(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap() else {
        panic!("expected Rows")
    };
    rows.iter()
        .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
        .collect()
}

fn streamed(e: &Engine, sql: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    e.execute_readonly_select_streaming(sql, CancelToken::none(), |item| {
        if let StreamItem::Row(cells) = item
            && let Value::Text(s) = cells.get(0).expect("first cell")
        {
            out.push(s.to_string());
        }
        Ok(())
    })
    .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    out
}

#[test]
fn a_composite_index_leading_on_the_order_column_is_named_as_a_walk() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE m (a int NOT NULL, b int NOT NULL, pad text)")
        .unwrap();
    for i in [1, 2, 3, 10] {
        e.execute(&format!("INSERT INTO m VALUES ({i}, {i}, 'x')"))
            .unwrap();
    }
    e.execute("CREATE INDEX m_ab ON m (a, b)").unwrap();
    let p = plan(&mut e, "EXPLAIN SELECT a FROM m ORDER BY a LIMIT 2");
    assert!(
        p.iter().any(|l| l.contains("Index Scan using m_ab")),
        "the walk was not named: {p:?}"
    );
    assert!(
        !p.iter().any(|l| l.trim_start().starts_with("Sort")),
        "a walked order still claimed a Sort: {p:?}"
    );
}

#[test]
fn the_executor_really_does_walk_it() {
    // The witness for the assertion above: the projection divides by
    // zero on the LAST row in key order, so an executor that reads
    // every row raises and one that stops after two does not. If this
    // ever raises, the plan above is naming something that is not
    // happening.
    let mut e = Engine::new();
    e.execute("CREATE TABLE m (a int NOT NULL, b int NOT NULL)")
        .unwrap();
    for i in [1, 2, 3, 10] {
        e.execute(&format!("INSERT INTO m VALUES ({i}, {i})"))
            .unwrap();
    }
    e.execute("CREATE INDEX m_ab ON m (a, b)").unwrap();
    let QueryResult::Rows { rows, .. } = e
        .execute("SELECT 30 / (a - 10) FROM m ORDER BY a LIMIT 2")
        .expect("stopping before a = 10 means never dividing by zero")
    else {
        panic!("expected Rows")
    };
    assert_eq!(rows.len(), 2);
    assert!(
        e.execute("SELECT 30 / (a - 10) FROM m ORDER BY a").is_err(),
        "the control: the offending row must be reachable"
    );
}

/// An index may make a query faster. It may never make it answer
/// differently — asked of both dialects, because only one of them
/// orders text by anything but bytes.
fn index_does_not_change_the_answer(mysql: bool) {
    let build = |with_index: bool| {
        let mut e = Engine::new();
        if mysql {
            e.set_mysql_dialect(true);
        }
        e.execute("CREATE TABLE s (t text NOT NULL)").unwrap();
        for v in ["alpha", "Beta", "GAMMA", "delta"] {
            e.execute(&format!("INSERT INTO s VALUES ('{v}')")).unwrap();
        }
        if with_index {
            e.execute("CREATE INDEX s_t ON s (t)").unwrap();
        }
        e
    };
    let without = streamed(&build(false), "SELECT t FROM s ORDER BY t");
    let with = build(true);
    assert_eq!(
        streamed(&with, "SELECT t FROM s ORDER BY t"),
        without,
        "mysql={mysql}: the index changed the order"
    );
    assert_eq!(
        streamed(&with, "SELECT t FROM s ORDER BY t LIMIT 2"),
        without[..2],
        "mysql={mysql}: the index changed the bounded order"
    );
}

#[test]
fn an_index_does_not_change_a_text_ordering_on_a_mysql_session() {
    // MySQL's default collation folds case, so its order is
    // `alpha Beta delta GAMMA` and the tree's byte order is not it.
    index_does_not_change_the_answer(true);
}

#[test]
fn nor_on_a_postgresql_session() {
    // The control. A `C`-collated PostgreSQL session orders by bytes,
    // which IS the tree's order, so this pair agreed even before the
    // guard — and it has to keep agreeing, or the guard has started
    // refusing walks it should take.
    index_does_not_change_the_answer(false);
}
