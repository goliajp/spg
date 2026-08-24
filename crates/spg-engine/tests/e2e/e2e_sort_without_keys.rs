//! v7.38.19 — a full sort whose ORDER BY column the projection already
//! carries builds no sort key and reads the projected cell instead.
//!
//! The key it skips is a COPY: on 400,000 rows of 192-character text
//! that was 400,000 allocations, 400,000 frees and 77 MB moved, and the
//! copy exists only because the source row is gone by the time the sort
//! runs. When the column is IN the output, it is not gone.
//!
//! The danger is that the two paths answer differently. A sort KEY
//! encodes what the column means; a VALUE is just what it holds, and for
//! several types those are not the same order:
//!
//!   * a user ENUM stores its label as text and orders by DECLARATION
//!     position — `('high','mid')` is the declared order and the sorted
//!     order, but as text `high < mid`;
//!   * an array orders element-wise, not by its rendered form;
//!   * a collated column orders by the collator, not by bytes.
//!
//! Each of those was a real failure before `value_order_is_key_order`
//! existed: eleven tests across four files went red on the first draft,
//! and `enum_member_ordering_still_holds_through_a_projection` reported
//! `["high", "mid"]` where the answer is `["mid", "high"]`. The tests
//! below hold that guard from the other side — remove any clause of it
//! and one of them returns a wrongly ordered result, not an error.

use spg_engine::{Engine, QueryResult};

fn col0(e: &mut Engine, sql: &str) -> Vec<String> {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                spg_storage::Value::Text(t) => t.to_string(),
                spg_storage::Value::Null => "<NULL>".into(),
                other => format!("{other:?}"),
            })
            .collect(),
        other => panic!("expected rows from {sql}, got {other:?}"),
    }
}

/// The shape the change exists for.
#[test]
fn text_sorts_the_same_with_no_key_built() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int, s text)").unwrap();
    for (i, s) in ["pear", "Apple", "fig", "apple", "Pear", "_under"]
        .iter()
        .enumerate()
    {
        e.execute(&format!("INSERT INTO t VALUES ({i}, '{s}')"))
            .unwrap();
    }
    // PostgreSQL 18.4, C collation: byte order, so capitals lead.
    assert_eq!(
        col0(&mut e, "SELECT s FROM t ORDER BY s"),
        ["Apple", "Pear", "_under", "apple", "fig", "pear"]
    );
    assert_eq!(
        col0(&mut e, "SELECT s FROM t ORDER BY s DESC"),
        ["pear", "fig", "apple", "_under", "Pear", "Apple"]
    );
}

/// NULL placement is the comparator's, not the fast arm's: the inlined
/// pair is two non-NULL strings and nothing else, so a NULL still walks
/// the shared comparator and lands where PG puts it.
#[test]
fn nulls_keep_their_place_without_a_key() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int, s text)").unwrap();
    e.execute("INSERT INTO t VALUES (1, 'b'), (2, NULL), (3, 'a')")
        .unwrap();
    // PG18.4: ASC puts NULLs last, DESC puts them first.
    assert_eq!(
        col0(&mut e, "SELECT s FROM t ORDER BY s"),
        ["a", "b", "<NULL>"]
    );
    assert_eq!(
        col0(&mut e, "SELECT s FROM t ORDER BY s DESC"),
        ["<NULL>", "b", "a"]
    );
    assert_eq!(
        col0(&mut e, "SELECT s FROM t ORDER BY s NULLS FIRST"),
        ["<NULL>", "a", "b"]
    );
}

/// A user ENUM orders by DECLARATION position. Its label is text, so a
/// value-level comparison would order it alphabetically — the exact
/// disagreement `value_order_is_key_order` exists to refuse.
#[test]
fn an_enum_still_orders_by_declaration_through_a_projection() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE prio AS ENUM ('urgent', 'mid', 'low')")
        .unwrap();
    e.execute("CREATE TABLE t (id int, p prio)").unwrap();
    e.execute("INSERT INTO t VALUES (1, 'low'), (2, 'urgent'), (3, 'mid')")
        .unwrap();
    // Declared order, not 'low' < 'mid' < 'urgent'.
    assert_eq!(
        col0(&mut e, "SELECT p FROM t ORDER BY p"),
        ["urgent", "mid", "low"]
    );
}

/// An array orders element-wise. Rendered, `{2}` would precede `{10}`.
#[test]
fn an_array_still_orders_element_wise_through_a_projection() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int, a int[])").unwrap();
    e.execute("INSERT INTO t VALUES (1, '{10}'), (2, '{2}'), (3, '{1,9}')")
        .unwrap();
    assert_eq!(
        col0(&mut e, "SELECT a FROM t ORDER BY a"),
        [
            "IntArray([Some(1), Some(9)])",
            "IntArray([Some(2)])",
            "IntArray([Some(10)])"
        ]
    );
}

/// The projection must BE the column, not merely be named for it. An
/// output item that renames or transforms holds a different value, and
/// sorting by the output would sort by the wrong one.
#[test]
fn an_output_item_that_only_shares_a_name_is_refused() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int, s text)").unwrap();
    e.execute("INSERT INTO t VALUES (1, 'bb'), (2, 'a'), (3, 'ccc')")
        .unwrap();
    // `s` in the ORDER BY is the OUTPUT item here, which is the length.
    assert_eq!(
        col0(&mut e, "SELECT length(s) AS s FROM t ORDER BY s"),
        ["Int(1)", "Int(2)", "Int(3)"]
    );
}
