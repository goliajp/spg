//! 9.0.0 — the regular expression engine takes POSIX's longest match, and
//! cuts a repetition the way PostgreSQL's captures report it.
//!
//! Reported by sentori (§4.3): where more than one alternative can match,
//! PostgreSQL prefers the longest and SPG took the one written first, so
//! `regexp_matches('foobar', '(fo|foo)(bar|obar)')` answered `{fo,obar}`
//! against `{foo,bar}`. Measuring the shape wider found a second defect:
//! a repetition never backtracked over how long each rep was, so
//! `'aaa' ~ '^(a|aa){3}$'` was false where PostgreSQL says true.
//!
//! Every expectation here is PostgreSQL 18.6's answer to the same
//! statement.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .unwrap_or_else(|| "<no rows>".into()),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn an_alternation_takes_the_longest_match() {
    let mut e = Engine::new();
    for (sql, want) in [
        (
            "SELECT regexp_matches('foobar', '(fo|foo)(bar|obar)')",
            "{foo,bar}",
        ),
        ("SELECT regexp_matches('xyz', '(x|xy)(z|yz)')", "{xy,z}"),
        ("SELECT regexp_matches('aaa', '(a|aa)(a|aa)')", "{aa,a}"),
        ("SELECT regexp_matches('abcd', '(a|ab)(c|bc)')", "{ab,c}"),
        (
            "SELECT regexp_matches('foobarbaz', '(foo|foobar)(baz|barbaz)')",
            "{foobar,baz}",
        ),
        ("SELECT substring('foobar' from 'fo|foo')", "foo"),
        ("SELECT substring('abcd' from 'a(b|bc)')", "bc"),
        ("SELECT regexp_replace('foobar', 'fo|foo', 'X')", "Xbar"),
        ("SELECT regexp_replace('abcd', 'ab|abc', 'X')", "Xd"),
        ("SELECT regexp_replace('aaa', 'a|aa', 'X', 'g')", "XX"),
        (
            "SELECT regexp_matches('123abc', '([0-9]+|[0-9]+[a-z]+)')",
            "{123abc}",
        ),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

/// A repetition can cover the same span several ways; PostgreSQL takes as
/// MANY reps as it can when the minimum is at least one, as FEW when it is
/// zero, and the earlier reps are the longer ones. The group keeps the
/// last rep, which is how the cut is visible.
#[test]
fn a_repetition_is_cut_the_way_postgres_reports_it() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT regexp_matches('aaaa', '(a|aa)+')", "{a}"),
        ("SELECT regexp_matches('aaaa', '(a|aa)*')", "{aa}"),
        ("SELECT regexp_matches('aaaaa', '(a|aa)*')", "{a}"),
        ("SELECT regexp_matches('aaaa', '(aa|a)+')", "{a}"),
        ("SELECT regexp_matches('aaaa', '(a|aa){1,3}')", "{a}"),
        ("SELECT regexp_matches('aaaa', '(a|aa){1,2}')", "{aa}"),
        ("SELECT regexp_matches('aaa', '(a|aa){3}')", "{a}"),
        ("SELECT regexp_matches('aaaaaa', '(a|aa|aaa)*')", "{aaa}"),
        ("SELECT regexp_matches('aaaa', '(a{1,2})+')", "{a}"),
        ("SELECT regexp_matches('aaaa', '((a)|(aa))+')", "{a,a,NULL}"),
        ("SELECT regexp_matches('abcab', '(abc|ab)+')", "{ab}"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

/// The count decides whether the repetition matches at all, not only what
/// it reports.
#[test]
fn a_counted_repetition_backtracks_over_rep_lengths() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT 'aaa' ~ '^(a|aa){3}$'", "true"),
        ("SELECT 'aaaa' ~ '^(aa|a){3}$'", "true"),
        ("SELECT 'abcab' ~ '^(abc|ab){2}$'", "true"),
        ("SELECT 'aa' ~ '^(a|aa){3}$'", "false"),
        ("SELECT regexp_replace('aaa', '^(a|aa){3}$', 'X')", "X"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}
