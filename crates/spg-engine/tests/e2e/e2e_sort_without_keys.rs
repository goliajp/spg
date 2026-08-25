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

/// v7.38.20 — a key that leaves long runs of ties is sorted run by run,
/// and a run that is NOT all-equal still gets sorted.
///
/// The shortcut is that a run whose values are all equal is already in
/// its stable order, which costs n-1 comparisons to prove instead of
/// n log n to re-establish. It is only sound when the run really is
/// uniform, so the interesting case is the one that is not: eight
/// leading bytes shared, and the ninth deciding.
#[test]
fn a_tied_run_that_is_not_uniform_is_still_ordered() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int, s text)").unwrap();
    // Every value shares the first eight bytes, so the prefix key ties
    // across all of them and the run covers the whole table.
    // Big enough that the sampler runs at all: `key_discriminates`
    // answers `true` for anything under eight sampled keys, so a
    // four-row fixture never reaches the path this test is for. The
    // first draft WAS four rows, and its negative control did not bite.
    for i in 0..400i32 {
        let tail = ["d", "b", "a", "c"][(i % 4) as usize];
        e.execute(&format!("INSERT INTO t VALUES ({i}, 'SHAREDPX{tail}')"))
            .unwrap();
    }
    let got = col0(&mut e, "SELECT s FROM t ORDER BY s");
    assert_eq!(got.len(), 400);
    assert_eq!(got[0], "SHAREDPXa");
    assert_eq!(got[399], "SHAREDPXd");
    assert!(got.windows(2).all(|w| w[0] <= w[1]), "not ordered");
    let desc = col0(&mut e, "SELECT s FROM t ORDER BY s DESC");
    assert_eq!(desc[0], "SHAREDPXd");
    assert_eq!(desc[399], "SHAREDPXa");
    assert!(desc.windows(2).all(|w| w[0] >= w[1]), "not ordered");
}

/// And a run that IS uniform keeps its input order, which is what
/// stability means. The `id` column rides along to make the order
/// visible.
#[test]
fn a_uniform_run_keeps_its_input_order() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int, s text)").unwrap();
    for i in 0..400i32 {
        e.execute(&format!("INSERT INTO t VALUES ({i}, 'SAMEVALUE')"))
            .unwrap();
    }
    // Every row ties; a stable sort must return them as they went in.
    match e.execute("SELECT s, id FROM t ORDER BY s").unwrap() {
        QueryResult::Rows { rows, .. } => {
            let ids: Vec<String> = rows.iter().map(|r| format!("{:?}", r.values[1])).collect();
            assert_eq!(
                ids,
                (0..400).map(|i| format!("Int({i})")).collect::<Vec<_>>()
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// v7.38.20 — a SECOND sort key no longer sends the whole sort to the
/// general path.
///
/// The inline-integer-key sort used to bail on `k.len() != 1`, saying
/// the tie rate was not knowable. The runs know it: sorting on the
/// first key leaves runs of equal first keys, and only those need the
/// later ones. These fixtures are the two shapes that matters between —
/// a first key that decides everything, and one that decides nothing.
#[test]
fn a_second_key_orders_within_the_first() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a int, b int, pad text)")
        .unwrap();
    // `a` repeats in blocks of ten, so every run is ten long and `b`
    // decides inside it. `b` descends on input so a working second key
    // has to move rows.
    for i in 0..400i32 {
        e.execute(&format!(
            "INSERT INTO t VALUES ({}, {}, 'p{i}')",
            i / 10,
            400 - i
        ))
        .unwrap();
    }
    // `pad` is what the projection carries, NOT the sort columns —
    // which is what keeps this on the key-based path in `orderby`
    // rather than the output-reading one above. A first draft selected
    // `a, b` and its negative control did not bite, because it was
    // exercising a different sort entirely.
    // NEITHER sort column is projected, which is what keeps this on the
    // key-based path rather than the output-reading one. `pad` carries
    // the input index so the order is still checkable.
    let got = col0(&mut e, "SELECT pad FROM t ORDER BY a, b");
    assert_eq!(got.len(), 400);
    // Row i has a = i/10 and b = 400-i, so inside each block of ten the
    // ascending `b` is the DESCENDING input index: the first block must
    // come back p9, p8, … p0.
    assert_eq!(
        &got[..10],
        &["p9", "p8", "p7", "p6", "p5", "p4", "p3", "p2", "p1", "p0"]
    );
    assert_eq!(got[10], "p19");
}

/// And when the first key decides everything — a permutation, every run
/// of length one — the second key must not disturb it.
#[test]
fn a_first_key_that_decides_needs_no_second_pass() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a int, b int, pad text)")
        .unwrap();
    for i in 0..400i64 {
        let a = (i * 7919) % 400;
        e.execute(&format!("INSERT INTO t VALUES ({a}, {}, 'p{i}')", 400 - i))
            .unwrap();
    }
    match e.execute("SELECT a, pad FROM t ORDER BY a, b").unwrap() {
        QueryResult::Rows { rows, .. } => {
            let got: Vec<i32> = rows
                .iter()
                .map(|r| match &r.values[0] {
                    spg_storage::Value::Int(a) => *a,
                    other => panic!("expected int, got {other:?}"),
                })
                .collect();
            assert_eq!(got, (0..400).collect::<Vec<i32>>());
        }
        other => panic!("expected rows, got {other:?}"),
    }
}
