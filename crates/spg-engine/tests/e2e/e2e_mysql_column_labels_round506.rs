//! v7.39 (round 506) — a MySQL session names an un-aliased column after the
//! SOURCE TEXT the client wrote, which is what MariaDB 11 does.
//!
//! Round 505 closed the PG half of this and left the MySQL half open,
//! because MariaDB's answer cannot be printed back out of a parsed shape:
//! the label is the text as WRITTEN, down to the spacing. `SELECT a  +  b`
//! names its column `a  +  b`; `SELECT COUNT( * )` names it `COUNT( * )`.
//! So the name is captured in the parser, from the byte offsets the lexer
//! already hands over, and filled into the item's alias — which every
//! downstream path already reports.
//!
//! Three rules and no more, all measured against MariaDB 11:
//!
//! | item        | label     | why                        |
//! |-------------|-----------|----------------------------|
//! | `lbl.a`     | `a`       | a column reports its name  |
//! | `'it''s'`   | `it's`    | a string reports its VALUE |
//! | `a  +  b`   | `a  +  b` | anything else, source text |
//!
//! Every expectation below is a MariaDB 11 reading.

use spg_engine::{Engine, QueryResult};

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e.execute("CREATE TABLE lbl (a INT, b INT, s VARCHAR(20))")
        .unwrap();
    e.execute("INSERT INTO lbl VALUES (1, 10, 'x'), (2, 20, 'y')")
        .unwrap();
    e
}

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

/// The ordinary shapes, where the label reads back as the item was written.
#[test]
fn round506_an_item_is_named_for_the_text_it_was_written_as() {
    let mut e = mysql();
    for (sql, want) in [
        ("SELECT count(*) FROM lbl", "count(*)"),
        ("SELECT sum(b) FROM lbl", "sum(b)"),
        ("SELECT upper(s) FROM lbl", "upper(s)"),
        ("SELECT concat(s,s) FROM lbl", "concat(s,s)"),
        ("SELECT a+b FROM lbl", "a+b"),
        ("SELECT a*2 FROM lbl", "a*2"),
        ("SELECT -a FROM lbl", "-a"),
        ("SELECT a IS NULL FROM lbl", "a IS NULL"),
        ("SELECT a>1 FROM lbl", "a>1"),
        ("SELECT 1+1", "1+1"),
        ("SELECT NULL", "NULL"),
        (
            "SELECT CASE WHEN a=1 THEN 'x' ELSE 'y' END FROM lbl",
            "CASE WHEN a=1 THEN 'x' ELSE 'y' END",
        ),
        (
            "SELECT (SELECT max(b) FROM lbl) FROM lbl",
            "(SELECT max(b) FROM lbl)",
        ),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

/// The spacing is the point: the label is the text as WRITTEN, so it cannot
/// come from printing the parsed shape back out.
#[test]
fn round506_the_label_keeps_the_spacing_the_client_wrote() {
    let mut e = mysql();
    for (sql, want) in [
        ("SELECT a  +  b FROM lbl", "a  +  b"),
        ("SELECT COUNT( * ) FROM lbl", "COUNT( * )"),
        ("SELECT   1   +   1", "1   +   1"),
        ("SELECT UPPER( s ) FROM lbl", "UPPER( s )"),
        // Case survives too — MariaDB does not fold it.
        ("SELECT COALESCE(a,0) FROM lbl", "COALESCE(a,0)"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

/// A column reports its NAME, not its source text: the qualifier is dropped.
/// A string literal reports its VALUE, escapes resolved.
#[test]
fn round506_columns_report_their_name_and_strings_their_value() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT a FROM lbl"), "a");
    assert_eq!(one(&mut e, "SELECT lbl.a FROM lbl"), "a");
    assert_eq!(one(&mut e, "SELECT 'lit'"), "lit");
    assert_eq!(one(&mut e, "SELECT 'it''s'"), "it's");
    // A numeric literal is not a string, so it keeps its source spelling.
    assert_eq!(labels(&mut e, "SELECT 1e3, 0x41"), vec!["1e3", "0x41"]);
}

/// An explicit alias always wins, and a derived table carries its inner
/// label outward.
#[test]
fn round506_aliases_win_and_derived_labels_propagate() {
    let mut e = mysql();
    assert_eq!(one(&mut e, "SELECT a AS alias1 FROM lbl"), "alias1");
    assert_eq!(one(&mut e, "SELECT a+b AS chosen FROM lbl"), "chosen");
    assert_eq!(one(&mut e, "SELECT * FROM (SELECT a+b FROM lbl) t"), "a+b");
    assert_eq!(
        labels(&mut e, "SELECT * FROM lbl"),
        vec!["a".to_string(), "b".to_string(), "s".to_string()]
    );
}

/// A PG session is untouched by any of this — it keeps round 505's rule.
#[test]
fn round506_a_pg_session_keeps_pgs_rule() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE lbl (a INT, b INT, s TEXT)")
        .unwrap();
    e.execute("INSERT INTO lbl VALUES (1, 10, 'x')").unwrap();
    assert_eq!(one(&mut e, "SELECT a+b FROM lbl"), "?column?");
    assert_eq!(one(&mut e, "SELECT upper(s) FROM lbl"), "upper");
    assert_eq!(one(&mut e, "SELECT count(*) FROM lbl"), "count");
    assert_eq!(one(&mut e, "SELECT 'lit'"), "?column?");
}

/// `parse_select_item` sits on the recursive select→item→expr→subquery
/// chain, and this round put two indices and a String on its frame — the
/// shape that blew the nesting stack in r413/r430. So: nest, and check both
/// that it runs and that the innermost label survives every wrapper.
///
/// The depth is deliberately small, and measuring why turned up something
/// this round did NOT cause. Nested derived tables are expensive on the
/// stack: parsing alone aborts a debug build at ~35 levels on an 8 MiB
/// stack, and EXECUTING them inside this harness's test thread aborts
/// around 8 — well before `MAX_NEST_DEPTH = 64` can turn it into an error.
/// The commit before this round aborts at the same depths, so it is
/// pre-existing; it is recorded rather than fixed here, because a test that
/// reached that depth would abort the whole binary instead of failing.
fn round506_the_label_survives_nesting() {
    const NEST: usize = 4;
    let mut e = mysql();
    let mut sql = String::from("SELECT a  +  b FROM lbl");
    for _ in 0..NEST {
        sql = format!("SELECT * FROM ({sql}) t");
    }
    assert_eq!(one(&mut e, &sql), "a  +  b");
}
