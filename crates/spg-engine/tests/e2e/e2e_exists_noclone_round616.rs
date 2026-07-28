//! v7.39 (round 616) — a predicate that is one EXISTS cloned the whole
//! subquery for every outer row.
//!
//! Round 596 decorrelated the correlated EXISTS whose outer side is an
//! expression, taking it from quadratic to linear. What was left is what the
//! splice costs: the planned EXISTS node is REPLACED by a boolean, and to
//! replace anything the row loop first clones the host expression — which,
//! when the host expression IS the `Expr::Exists`, clones the entire
//! subquery AST with it, once per outer row. Counted over 100k rows:
//!
//!     EXISTS (… b.id = a.id)       1 allocation a row    12.2 ms
//!     EXISTS (… b.id = a.id + 1)  18.16                  64.3
//!     NOT EXISTS (… b.id = a.id+1) 19.16                 66.7
//!
//! When the whole predicate is that node there is nothing to splice into, so
//! the verdict is read straight from the plan and handed back. `NOT EXISTS`
//! arrives as a `Not` wrapping the node rather than as its `negated` flag,
//! so both spellings are read; and the check happens before the plan is
//! cloned out of the memo, so the row loop does not copy that either.
//!
//!     EXISTS (… b.id = a.id + 1)   18.16 -> 5.16 allocations a row  64.3 -> 39.7 ms
//!     NOT EXISTS (… b.id = a.id+1) 19.16 -> 5.16                    66.7 -> 41.1
//!
//! and over pgwire on 500k rows against PG18:
//!
//!     NOT EXISTS (… b.id = a.id + 500000)  343.42 -> 210.61  PG 38.27  9.49x -> 5.50x
//!     EXISTS     (… b.id = a.id + 500000)  336.69 -> 211.84  PG 31.26 10.33x -> 6.78x
//!
//! The shortcut only applies when the predicate is exactly that node, so the
//! pins carry the same EXISTS through the shapes that must still take the
//! splice — beside another conjunct, under an OR, under an explicit NOT,
//! under `IS NOT TRUE`, and in the select list rather than the WHERE — and
//! check they answer the same. All 18 shapes were checked against live PG18
//! and matched byte for byte, the NULL rules included: a NULL correlation
//! key matches nothing, so EXISTS is false and NOT EXISTS is true for it.
//!
//! Measured and NOT closed: 5.16 allocations a row remain — the key vector,
//! its canonical encoding, and three more that were not located.

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

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ea (id INT, g INT, s TEXT)").unwrap();
    e.execute("CREATE TABLE eb (id INT, g INT, s TEXT)").unwrap();
    e.execute("INSERT INTO ea VALUES (1,10,'a'),(2,20,'b'),(3,NULL,NULL),(NULL,30,'c'),(4,10,'a')")
        .unwrap();
    e.execute("INSERT INTO eb VALUES (2,10,'a'),(3,20,'x'),(NULL,40,NULL),(5,10,'a')")
        .unwrap();
    e
}

/// The shape that takes the shortcut, in both spellings and over both a
/// plain and a computed correlation key.
#[test]
fn round616_bare_exists_and_not_exists() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT id FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE b.id = a.id) ORDER BY 1 NULLS LAST"),
        vec!["2", "3"]
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM ea a WHERE NOT EXISTS (SELECT 1 FROM eb b WHERE b.id = a.id) ORDER BY 1 NULLS LAST"),
        vec!["1", "4", "NULL"],
        "the NULL-keyed row matches nothing, so NOT EXISTS keeps it"
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE b.id = a.id + 1) ORDER BY 1 NULLS LAST"),
        vec!["1", "2", "4"],
        "a computed correlation key — the shape round 596 decorrelated"
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM ea a WHERE NOT EXISTS (SELECT 1 FROM eb b WHERE b.id = a.id + 1) ORDER BY 1 NULLS LAST"),
        vec!["3", "NULL"]
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE b.g = a.g) ORDER BY 1 NULLS LAST"),
        vec!["1", "2", "4"]
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM ea a WHERE NOT EXISTS (SELECT 1 FROM eb b WHERE b.g = a.g) ORDER BY 1 NULLS LAST"),
        vec!["3", "NULL"]
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE b.s = a.s) ORDER BY 1 NULLS LAST"),
        vec!["1", "4"],
        "a text correlation key"
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM ea a WHERE NOT EXISTS (SELECT 1 FROM eb b WHERE b.g = a.g + 30) ORDER BY 1 NULLS LAST"),
        vec!["2", "3", "NULL"]
    );
}

/// The shapes the shortcut must NOT claim — they still take the splice, and
/// have to answer the same.
#[test]
fn round616_exists_inside_a_larger_predicate() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT id FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE b.id = a.id) AND a.g = 10 ORDER BY 1 NULLS LAST"),
        Vec::<String>::new(),
        "beside another conjunct"
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM ea a WHERE a.g = 10 OR EXISTS (SELECT 1 FROM eb b WHERE b.id = a.id) ORDER BY 1 NULLS LAST"),
        vec!["1", "2", "3", "4"],
        "under an OR"
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM ea a WHERE NOT (EXISTS (SELECT 1 FROM eb b WHERE b.id = a.id)) ORDER BY 1 NULLS LAST"),
        vec!["1", "4", "NULL"],
        "an explicit NOT around it — which IS the shortcut's second spelling"
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE b.id = a.id) IS NOT TRUE ORDER BY 1 NULLS LAST"),
        vec!["1", "4", "NULL"],
        "under a boolean test"
    );
    assert_eq!(
        vals(&mut e, "SELECT id, EXISTS (SELECT 1 FROM eb b WHERE b.id = a.id) FROM ea a ORDER BY 1 NULLS LAST"),
        vec!["1|false", "2|true", "3|true", "4|false", "NULL|false"],
        "in the select list, where there is no predicate at all"
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE b.id = a.id AND b.g = a.g) ORDER BY 1 NULLS LAST"),
        Vec::<String>::new(),
        "two correlation keys"
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE b.id = a.id AND b.s = 'a') ORDER BY 1 NULLS LAST"),
        vec!["2"],
        "a correlated key beside an uncorrelated filter"
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE b.id = a.id * 1)"),
        vec!["2"]
    );
}

/// An inner side that matches nothing, and one that matches everything.
#[test]
fn round616_degenerate_inner() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT id FROM ea a WHERE EXISTS (SELECT 1 FROM eb b WHERE FALSE) ORDER BY 1 NULLS LAST"),
        Vec::<String>::new()
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM ea a WHERE NOT EXISTS (SELECT 1 FROM eb b WHERE FALSE) ORDER BY 1 NULLS LAST"),
        vec!["1", "2", "3", "4", "NULL"]
    );
}

/// At the size where the subquery was being cloned half a million times.
#[test]
fn round616_scale() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ba (id INT)").unwrap();
    e.execute("CREATE TABLE bb (id INT)").unwrap();
    e.execute("INSERT INTO ba SELECT gg FROM generate_series(1, 20000) gg").unwrap();
    e.execute("INSERT INTO bb SELECT gg FROM generate_series(10000, 30000) gg").unwrap();
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM ba a WHERE EXISTS (SELECT 1 FROM bb b WHERE b.id = a.id)"),
        vec!["10001"]
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM ba a WHERE NOT EXISTS (SELECT 1 FROM bb b WHERE b.id = a.id)"),
        vec!["9999"],
        "and the two partition the table"
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM ba a WHERE EXISTS (SELECT 1 FROM bb b WHERE b.id = a.id + 10000)"),
        vec!["20000"]
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM ba a WHERE EXISTS (SELECT 1 FROM bb b WHERE b.id = a.id)"),
        vals(&mut e, "SELECT count(*) FROM ba a WHERE a.id IN (SELECT b.id FROM bb b)"),
        "the EXISTS and the IN spellings agree"
    );
}
