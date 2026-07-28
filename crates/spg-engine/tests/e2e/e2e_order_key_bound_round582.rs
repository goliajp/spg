//! v7.39 (round 582) — the ORDER BY column was resolved by name, once
//! per row.
//!
//! Round 581 left `ORDER BY … LIMIT` at 2.02x for one key and 2.92x for
//! two. Profiling the two-key form put 8% of the query in
//! `resolve_column` alone — looking up the same two names, by string
//! comparison, half a million times each — with `build_order_keys_into`
//! another 8% around it. The position of a column does not change
//! between rows.
//!
//! `order_by_bound_positions` resolves each key once, before the scan.
//! A key that is a bare column of the scanned relation reads its cell;
//! anything else — an expression, a qualified name belonging to another
//! relation, an ambiguous bare name — keeps the interpretive path, so
//! the keys are the ones the resolver would have produced.
//!
//! Engine-side, 500k rows:
//!
//!     ORDER BY g DESC, id DESC LIMIT 10   19.44 -> 10.48 ms   -46%
//!     ORDER BY id LIMIT 10                12.18 ->  7.82      -36%
//!     ORDER BY id DESC LIMIT 10           20.20 -> 15.88      -21%
//!     ORDER BY id DESC LIMIT 1000         23.87 -> 19.67      -18%
//!     ORDER BY id + 1 DESC LIMIT 10       58.99 -> 59.36      noise
//!
//! The expression row is there because the first attempt DID cost it
//! 3.9%: routing both paths through a borrowed value left the
//! interpretive one cloning its result. Reading the cell by reference
//! and evaluating by value removed the clone from both.
//!
//! Over pgwire in a warm session against PG18:
//!
//!     ORDER BY id DESC LIMIT 10       21.50 -> 16.76   PG 10.67   2.01x -> 1.57x
//!     ORDER BY g DESC, id DESC L10    20.59 -> 11.96   PG  7.62   2.70x -> 1.57x
//!     ORDER BY id LIMIT 10            13.15 ->  9.24   PG  6.71   1.96x -> 1.38x
//!
//! The risk in resolving a name early is resolving the WRONG one, so the
//! pins below cover every way an ORDER BY name can mean something other
//! than the table's column of that name.

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

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE s582 (id INT, g INT, t TEXT)").unwrap();
    e.execute(
        "INSERT INTO s582 SELECT gg, 1000 - gg, 'r' || gg FROM generate_series(1, 3000) gg",
    )
    .unwrap();
    e
}

/// An output alias that shadows a table column names the OUTPUT.
#[test]
fn round582_output_alias_shadows_a_table_column() {
    let mut e = engine();
    assert_eq!(
        vals(&mut e, "SELECT g AS id FROM s582 ORDER BY id DESC LIMIT 3"),
        vec!["999", "998", "997"]
    );
    assert_eq!(
        vals(&mut e, "SELECT g AS id FROM s582 ORDER BY id LIMIT 3"),
        vec!["-2000", "-1999", "-1998"]
    );
    // An ordinal names the output position.
    assert_eq!(
        vals(&mut e, "SELECT g FROM s582 ORDER BY 1 DESC LIMIT 3"),
        vec!["999", "998", "997"]
    );
}

/// A qualifier that matches the scanned relation binds; one that does
/// not must not.
#[test]
fn round582_qualifiers() {
    let mut e = engine();
    assert_eq!(
        vals(&mut e, "SELECT id FROM s582 ORDER BY s582.id DESC LIMIT 3"),
        vec!["3000", "2999", "2998"]
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM s582 x ORDER BY x.id DESC LIMIT 3"),
        vec!["3000", "2999", "2998"],
        "the alias, not the table name"
    );
    // Case-insensitively, as SQL names are.
    assert_eq!(
        vals(&mut e, "SELECT id FROM s582 ORDER BY ID DESC LIMIT 3"),
        vec!["3000", "2999", "2998"]
    );
}

/// Expressions, functions and a key that is not projected at all keep
/// the interpretive path and their own answers.
#[test]
fn round582_expressions_keep_the_old_path() {
    let mut e = engine();
    assert_eq!(
        vals(&mut e, "SELECT id FROM s582 ORDER BY id + 1 DESC LIMIT 3"),
        vec!["3000", "2999", "2998"]
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM s582 ORDER BY -id LIMIT 3"),
        vec!["3000", "2999", "2998"]
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM s582 ORDER BY length(t) DESC, id LIMIT 3"),
        vec!["1000", "1001", "1002"],
        "four-character labels first, then by id"
    );
    // Ordering by a column that is not projected.
    assert_eq!(
        vals(&mut e, "SELECT id FROM s582 ORDER BY g DESC LIMIT 3"),
        vec!["1", "2", "3"]
    );
    // A CASE expression, and a mixed key list.
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM s582 ORDER BY CASE WHEN id % 2 = 0 THEN 0 ELSE 1 END, id DESC LIMIT 3"
        ),
        vec!["3000", "2998", "2996"]
    );
}

/// Every type the key builder handles specially still sorts the same
/// way when its cell is read directly.
#[test]
fn round582_types_sort_the_same_read_directly() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE t582 (i INT, b BIGINT, f FLOAT, n NUMERIC, s TEXT, d DATE, o BOOL)",
    )
    .unwrap();
    e.execute(
        "INSERT INTO t582 SELECT gg, gg::BIGINT * 1000000000, gg / 7.0, gg * 1.5, 'r' || gg, \
         DATE '2026-01-01' + gg, gg % 2 = 0 FROM generate_series(1, 2000) gg",
    )
    .unwrap();
    assert_eq!(vals(&mut e, "SELECT i FROM t582 ORDER BY i DESC LIMIT 2"), vec!["2000", "1999"]);
    assert_eq!(
        vals(&mut e, "SELECT i FROM t582 ORDER BY b DESC LIMIT 2"),
        vec!["2000", "1999"]
    );
    assert_eq!(
        vals(&mut e, "SELECT i FROM t582 ORDER BY f DESC LIMIT 2"),
        vec!["2000", "1999"]
    );
    assert_eq!(
        vals(&mut e, "SELECT i FROM t582 ORDER BY n DESC LIMIT 2"),
        vec!["2000", "1999"]
    );
    assert_eq!(
        vals(&mut e, "SELECT s FROM t582 ORDER BY s DESC LIMIT 2"),
        vec!["r999", "r998"],
        "text sorts lexicographically"
    );
    assert_eq!(
        vals(&mut e, "SELECT i FROM t582 ORDER BY d DESC LIMIT 2"),
        vec!["2000", "1999"]
    );
    assert_eq!(
        vals(&mut e, "SELECT o FROM t582 ORDER BY o DESC, i LIMIT 2"),
        vec!["true", "true"]
    );
}

/// An enum key sorts by catalog member order, and that rule survives
/// reading the cell directly.
#[test]
fn round582_enum_keys_keep_member_order() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE mood582 AS ENUM ('sad', 'ok', 'happy')")
        .unwrap();
    e.execute("CREATE TABLE m582 (id INT, m mood582)").unwrap();
    e.execute(
        "INSERT INTO m582 SELECT gg, (ARRAY['sad','ok','happy'])[1 + gg % 3]::mood582 \
         FROM generate_series(1, 2000) gg",
    )
    .unwrap();
    let got = vals(&mut e, "SELECT m FROM m582 ORDER BY m DESC LIMIT 3");
    assert!(got.iter().all(|v| v == "happy"), "{got:?}");
    let got = vals(&mut e, "SELECT m FROM m582 ORDER BY m LIMIT 3");
    assert!(got.iter().all(|v| v == "sad"), "{got:?}");
}
