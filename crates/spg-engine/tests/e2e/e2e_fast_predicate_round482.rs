//! read01 round 482 — the `<column> <cmp> <literal>` fast predicate must
//! answer exactly what the general path answers.
//!
//! Rounds 478-481 traced the per-row predicate cost to `Value` churn: three
//! steps a row means three `Value`s built and destroyed, and
//! `drop_glue<Value>` is an out-of-line call even for a value carrying no
//! heap. This shape needs none of them — both operands can be read by
//! reference — and it is what `g = 5` and `s = '…'` compile to.
//!
//! The fast path calls `apply_binary_by_ref`, the same function
//! `Step::Binary` reaches for first, so the answer is identical by
//! construction. This pins that anyway, across the cases where a
//! comparison is not simply true or false: NULL on either side, a text /
//! number coercion, and the boundary values.

use spg_engine::{Engine, QueryResult};

fn n_rows(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
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
            .join(";"),
        other => panic!("{sql} -> {other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT, g INT, s TEXT)").unwrap();
    e.execute(
        "INSERT INTO t VALUES (1, 5, 'five'), (2, 7, 'seven'), (3, NULL, NULL), \
         (4, 5, 'five'), (5, -1, '')",
    )
    .unwrap();
    e
}

/// Predicates that take the fast path, each with the count the general
/// path produced before it existed.
const CASES: &[(&str, &str)] = &[
    ("SELECT count(*) FROM t WHERE g = 5", "2"),
    ("SELECT count(*) FROM t WHERE g <> 5", "2"),
    ("SELECT count(*) FROM t WHERE g < 5", "1"),
    ("SELECT count(*) FROM t WHERE g <= 5", "3"),
    ("SELECT count(*) FROM t WHERE g > 5", "1"),
    ("SELECT count(*) FROM t WHERE g >= 5", "3"),
    // A NULL cell must not match anything, including inequality.
    ("SELECT count(*) FROM t WHERE g = 999", "0"),
    ("SELECT count(*) FROM t WHERE g <> 999", "4"),
    ("SELECT count(*) FROM t WHERE s = 'five'", "2"),
    ("SELECT count(*) FROM t WHERE s = ''", "1"),
    ("SELECT count(*) FROM t WHERE s > 'seven'", "0"),
    // PG coerces an unknown-type literal to the column's type.
    ("SELECT count(*) FROM t WHERE g = '5'", "2"),
    ("SELECT count(*) FROM t WHERE g < '0'", "1"),
];

#[test]
fn round482_the_fast_predicate_answers_what_the_general_path_answers() {
    let mut e = seeded();
    for (sql, want) in CASES {
        assert_eq!(n_rows(&mut e, sql).as_str(), *want, "for `{sql}`");
    }
}

#[test]
fn round482_the_rows_it_selects_are_the_same_rows() {
    // A count can agree while the wrong rows are chosen.
    let mut e = seeded();
    assert_eq!(
        n_rows(&mut e, "SELECT id FROM t WHERE g = 5 ORDER BY id"),
        "1;4"
    );
    assert_eq!(
        n_rows(&mut e, "SELECT id FROM t WHERE g <> 5 ORDER BY id"),
        "2;5"
    );
    assert_eq!(
        n_rows(&mut e, "SELECT id FROM t WHERE s = 'five' ORDER BY id"),
        "1;4"
    );
}

#[test]
fn round482_shapes_it_must_decline_still_work() {
    // Not `<column> <cmp> <literal>`: the general path has to take these,
    // and a wrong shape match would silently change the answer.
    let mut e = seeded();
    // literal on the left (operator would need flipping — declined)
    assert_eq!(n_rows(&mut e, "SELECT count(*) FROM t WHERE 5 = g"), "2");
    // column vs column — no row has id equal to g, and one has id > g
    // (id 5, g -1), which is what makes this pair worth asserting.
    assert_eq!(n_rows(&mut e, "SELECT count(*) FROM t WHERE id = g"), "0");
    assert_eq!(n_rows(&mut e, "SELECT count(*) FROM t WHERE id > g"), "1");
    // an arithmetic operand
    assert_eq!(n_rows(&mut e, "SELECT count(*) FROM t WHERE g + 1 = 6"), "2");
    // a compound predicate
    assert_eq!(
        n_rows(&mut e, "SELECT count(*) FROM t WHERE g = 5 AND id > 1"),
        "1"
    );
    // LIKE is its own AST node, not a BinOp — general path
    assert_eq!(
        n_rows(&mut e, "SELECT count(*) FROM t WHERE s LIKE 'fi%'"),
        "2"
    );
}

#[test]
fn round482_mysql_truthiness_is_unchanged() {
    // The fast path ends in `predicate_is_true`, the same call the general
    // path makes, so a MySQL session keeps its own reading.
    let mut e = Engine::new();
    e.set_backslash_escapes(true);
    e.execute("CREATE TABLE m (a INT)").unwrap();
    e.execute("INSERT INTO m VALUES (1),(2),(3)").unwrap();
    assert_eq!(n_rows(&mut e, "SELECT count(*) FROM m WHERE a = 2"), "1");
    assert_eq!(n_rows(&mut e, "SELECT count(*) FROM m WHERE a"), "3");
}
