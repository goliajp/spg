//! v7.39 (round 505) — the column name a projected expression reports when
//! the query gives it no `AS` alias.
//!
//! SPG printed the parsed expression back out, which matched neither oracle,
//! so name-keyed row access — every ORM, every `row["upper"]` — missed:
//!
//! | query                  | PG18       | MariaDB 11  | SPG (before) |
//! |------------------------|------------|-------------|--------------|
//! | `upper(s)`             | `upper`    | `upper(s)`  | `upper(s)`   |
//! | `string_agg(s, ',')`   | `string_agg` | —         | `string_agg(s, ',')` |
//! | `a+b`                  | `?column?` | `a+b`       | `(a + b)`    |
//! | `'lit'`                | `?column?` | `lit`       | `'lit'`      |
//! | `CASE …`               | `case`     | source text | `CASE WHEN (a = 1) THEN …` |
//! | `count(*) OVER ()`     | `count`    | —           | `__win_0`    |
//! | `EXISTS(SELECT 1)`     | `exists`   | —           | `?column?`   |
//! | `INTERVAL '1 day'`     | `interval` | —           | `?column?`   |
//!
//! Two of those were not one bug but four, in four different places, each
//! losing the name for its own reason: the aggregate projection had its own
//! naming code; the window rewrite swapped the call for a synthetic
//! `__win_N` column and let the projection report the internal name; and an
//! uncorrelated subquery was replaced by its VALUE before the projection
//! ever saw the shape it was named for.
//!
//! Nothing in the suite pinned any of this, which is why it could drift
//! this far from both oracles. Every expectation below is a PG18 reading.
//!
//! The MySQL half is MariaDB's own rule — the item's SOURCE TEXT verbatim,
//! `SELECT a  +  b` reporting `a  +  b`, spacing and all — and round 506
//! closed it from the parser's byte offsets. See
//! `e2e_mysql_column_labels_round506`.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE lbl (a INT, b INT, s TEXT)").unwrap();
    e.execute("INSERT INTO lbl VALUES (1, 10, 'x')").unwrap();
    e
}

/// The column names a query reports, in order.
fn labels(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { columns, .. } => columns.iter().map(|c| c.name.clone()).collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn one(e: &mut Engine, sql: &str) -> String {
    let l = labels(e, sql);
    assert_eq!(l.len(), 1, "{sql} should report one column, got {l:?}");
    l.into_iter().next().unwrap()
}

/// A call is named for its function — the single most load-bearing rule
/// here, because it is what an ORM reads back.
#[test]
fn round505_a_call_is_named_for_its_function() {
    let mut e = engine();
    for (sql, want) in [
        ("SELECT upper(s) FROM lbl", "upper"),
        ("SELECT length(s) FROM lbl", "length"),
        ("SELECT concat(s, s) FROM lbl", "concat"),
        ("SELECT coalesce(a, 0) FROM lbl", "coalesce"),
        ("SELECT nullif(a, 1) FROM lbl", "nullif"),
        ("SELECT greatest(a, b) FROM lbl", "greatest"),
        ("SELECT least(a, b) FROM lbl", "least"),
        // Shapes that only LOOK like syntax: PG resolves them to
        // functions before naming them.
        ("SELECT EXTRACT(year FROM DATE '2020-01-01')", "extract"),
        ("SELECT EXISTS(SELECT 1) FROM lbl", "exists"),
        ("SELECT ARRAY[1, 2]", "array"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

/// Aggregates go through their own projection, and it had its own naming.
#[test]
fn round505_an_aggregate_is_named_for_its_function() {
    let mut e = engine();
    for (sql, want) in [
        ("SELECT count(*) FROM lbl", "count"),
        ("SELECT sum(b) FROM lbl", "sum"),
        ("SELECT avg(b) FROM lbl", "avg"),
        ("SELECT min(a) FROM lbl", "min"),
        ("SELECT max(a) FROM lbl", "max"),
        ("SELECT string_agg(s, ',') FROM lbl", "string_agg"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
    // A grouped query names the group column and the aggregate separately.
    assert_eq!(
        labels(&mut e, "SELECT a, count(*) FROM lbl GROUP BY a"),
        vec!["a".to_string(), "count".to_string()]
    );
}

/// A window call reports its function, not the synthetic column the rewrite
/// puts in its place. `__win_0` was an internal name reaching the client.
#[test]
fn round505_a_window_call_reports_its_function_not_the_synthetic_column() {
    let mut e = engine();
    for (sql, want) in [
        ("SELECT count(*) OVER () FROM lbl", "count"),
        ("SELECT row_number() OVER (ORDER BY a) FROM lbl", "row_number"),
        ("SELECT sum(b) OVER () FROM lbl", "sum"),
    ] {
        let got = one(&mut e, sql);
        assert!(
            !got.starts_with("__win"),
            "{sql} leaked the internal name {got}"
        );
        assert_eq!(got, want, "{sql}");
    }
}

/// Operators, comparisons and bare literals have no name at all.
#[test]
fn round505_operators_and_literals_report_no_name() {
    let mut e = engine();
    for sql in [
        "SELECT a + b FROM lbl",
        "SELECT a * 2 FROM lbl",
        "SELECT -a FROM lbl",
        "SELECT a > 1 FROM lbl",
        "SELECT a IS NULL FROM lbl",
        "SELECT s LIKE 'a%' FROM lbl",
        "SELECT a IN (1, 2) FROM lbl",
        "SELECT a BETWEEN 1 AND 2 FROM lbl",
        "SELECT 1",
        "SELECT 'lit'",
        "SELECT NULL",
    ] {
        assert_eq!(one(&mut e, sql), "?column?", "{sql}");
    }
}

/// A cast prefers its argument's name and settles for the type — the rule
/// that took a measurement to get right, because `upper(s)::text` and
/// `(CASE …)::text` disagree even though a bare `CASE …` does report `case`.
#[test]
fn round505_a_cast_prefers_its_argument_then_the_type() {
    let mut e = engine();
    for (sql, want) in [
        ("SELECT CAST(a AS TEXT) FROM lbl", "a"),
        ("SELECT a::text FROM lbl", "a"),
        ("SELECT a::text::int FROM lbl", "a"),
        ("SELECT upper(s)::text FROM lbl", "upper"),
        ("SELECT (a + b)::text FROM lbl", "text"),
        ("SELECT 1::text", "text"),
        ("SELECT (CASE WHEN a = 1 THEN 1 END)::text FROM lbl", "text"),
        // A typed literal names itself for its type, and weakly — the
        // cast around it wins.
        ("SELECT INTERVAL '1 day'", "interval"),
        ("SELECT (INTERVAL '1 day')::text", "text"),
        ("SELECT DATE '2020-01-01'", "date"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
    // On its own, CASE names itself.
    assert_eq!(
        one(&mut e, "SELECT CASE WHEN a = 1 THEN 1 END FROM lbl"),
        "case"
    );
}

/// A scalar subquery reports whatever its single output column reports —
/// even though the subquery itself is gone by projection time.
#[test]
fn round505_a_scalar_subquery_takes_its_columns_name() {
    let mut e = engine();
    for (sql, want) in [
        ("SELECT (SELECT max(b) FROM lbl)", "max"),
        ("SELECT (SELECT max(b) FROM lbl) FROM lbl", "max"),
        ("SELECT (SELECT b AS chosen FROM lbl LIMIT 1)", "chosen"),
        ("SELECT (SELECT a + b FROM lbl LIMIT 1)", "?column?"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

/// A column keeps its own name, qualifier discarded; an explicit alias
/// always wins.
#[test]
fn round505_columns_and_explicit_aliases_are_unchanged() {
    let mut e = engine();
    assert_eq!(one(&mut e, "SELECT a FROM lbl"), "a");
    assert_eq!(one(&mut e, "SELECT lbl.a FROM lbl"), "a");
    assert_eq!(one(&mut e, "SELECT a AS chosen FROM lbl"), "chosen");
    assert_eq!(one(&mut e, "SELECT a + b AS chosen FROM lbl"), "chosen");
    assert_eq!(one(&mut e, "SELECT count(*) AS chosen FROM lbl"), "chosen");
    assert_eq!(
        labels(&mut e, "SELECT * FROM lbl"),
        vec!["a".to_string(), "b".to_string(), "s".to_string()]
    );
}

/// The rule is PG's, and a MySQL session does not take it: MariaDB has its
/// own answer, which round 506 implemented. This pins only that the two
/// dialects stay apart here; the MariaDB values themselves are pinned in
/// `e2e_mysql_column_labels_round506`.
#[test]
fn round505_the_mysql_dialect_does_not_take_pgs_answer() {
    let mut e = engine();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    assert_eq!(one(&mut e, "SELECT a + b FROM lbl"), "a + b");
    assert_eq!(one(&mut e, "SELECT upper(s) FROM lbl"), "upper(s)");
}
