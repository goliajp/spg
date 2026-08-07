//! v7.39 (round 257) — the array surface, swept 125 cases against live
//! PG18.4 (2026-07-19). The function family was already solid (67/69 on
//! the first pass: position/positions/remove/replace/fill, the length
//! and dimension family, cat/append/prepend, to_string/to_array,
//! subscripts and slices, the comparison and containment operators,
//! ANY/ALL, unnest). The gaps:
//!
//!   * `array_position(arr, elem, start)` — the three-argument form —
//!     was missing entirely.
//!   * `UPDATE … SET a[lo:hi] = src` (slice assignment) did not parse.
//!     PG's rules, all probed: the slice is replaced in place; a source
//!     SHORTER than the slice is an error ("source array too small"); a
//!     longer one is truncated; a slice past the end extends the array
//!     with a NULL-padded hole; a NULL array becomes a fresh one; and
//!     `a[lo:]` runs to the end.
//!   * `array_agg(DISTINCT x)` kept first-seen order. PG dedups by
//!     SORTING, so the collection aggregates (array_agg / string_agg /
//!     json_agg) emit sorted values, NULLs last. Two older pins locked
//!     SPG's order as an accepted divergence — reasoning that SQL leaves
//!     it unspecified — but a program ported from PG sees the
//!     difference, so they now assert PG's order.
//!
//! Recorded epic, with a measured cost rather than a hand-wave:
//! non-default array LOWER BOUNDS are unsupported. `'[5:7]={1,2,3}'`
//! does not parse, `array_lower` is always 1, `array_dims` always
//! `[1:n]`, `array_fill`'s third argument is ignored, and PG counts
//! bounds in equality (`'[5:7]={1,2,3}' = ARRAY[1,2,3]` is FALSE).
//! Supporting them means adding a lower bound to all 18 array Value
//! variants — `IntArray` alone has 189 construction/match sites in
//! spg-engine — plus the storage format and the wire.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            // Speak the oracle's dialect: psql -tA prints booleans t / f.
            spg_storage::Value::Bool(b) => String::from(if *b { "t" } else { "f" }),
            other => spg_engine::eval::value_to_text(other),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

#[test]
fn array_position_takes_a_start_subscript() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT array_position(ARRAY[10,20,30,20], 20, 3)", "4"),
        ("SELECT array_position(ARRAY[10,20,30,20], 20, 1)", "2"),
        // A start past the end finds nothing.
        ("SELECT array_position(ARRAY[10,20,30,20], 20, 5)", "NULL"),
        ("SELECT array_position(ARRAY['a','b','a'], 'a', 2)", "3"),
        // Below 1 simply starts at the front.
        ("SELECT array_position(ARRAY[1,2,3], 3, 0)", "3"),
        // The two-argument form is unchanged.
        ("SELECT array_position(ARRAY[10,20,30,20], 20)", "2"),
        ("SELECT array_position(ARRAY[10,20,30], 99)", "NULL"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
    let got = err(&mut e, "SELECT array_position(ARRAY[1,2,3], 1, NULL)");
    assert!(got.contains("initial position must not be null"), "{got}");
}

#[test]
fn slice_assignment_follows_pgs_rules() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE sl (id int, a int[])").unwrap();
    e.execute("INSERT INTO sl VALUES (1, ARRAY[1,2,3,4,5])")
        .unwrap();
    // In-place replacement.
    e.execute("UPDATE sl SET a[2:3] = ARRAY[20,30] WHERE id=1")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT a FROM sl WHERE id=1"), "{1,20,30,4,5}");
    // A source shorter than the slice is refused, and changes nothing.
    let got = err(&mut e, "UPDATE sl SET a[2:3] = ARRAY[7] WHERE id=1");
    assert!(got.contains("source array too small"), "{got}");
    assert_eq!(one(&mut e, "SELECT a FROM sl WHERE id=1"), "{1,20,30,4,5}");
    // A longer source is truncated to the slice.
    e.execute("UPDATE sl SET a[2:3] = ARRAY[8,9,10] WHERE id=1")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT a FROM sl WHERE id=1"), "{1,8,9,4,5}");
    // A slice past the end extends, NULL-padding the hole.
    e.execute("UPDATE sl SET a[7:8] = ARRAY[70,80] WHERE id=1")
        .unwrap();
    assert_eq!(
        one(&mut e, "SELECT a FROM sl WHERE id=1"),
        "{1,8,9,4,5,NULL,70,80}"
    );
    // A NULL array becomes a fresh one.
    e.execute("INSERT INTO sl VALUES (2, NULL)").unwrap();
    e.execute("UPDATE sl SET a[1:2] = ARRAY[1,2] WHERE id=2")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT a FROM sl WHERE id=2"), "{1,2}");
    // The open form runs to the end.
    e.execute("INSERT INTO sl VALUES (3, ARRAY[1,2,3])")
        .unwrap();
    e.execute("UPDATE sl SET a[2:] = ARRAY[9,9] WHERE id=3")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT a FROM sl WHERE id=3"), "{1,9,9}");
    // Several subscript writes to one column still merge into one array.
    e.execute("UPDATE sl SET a[1] = 100, a[3] = 300 WHERE id=3")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT a FROM sl WHERE id=3"), "{100,9,300}");
}

#[test]
fn distinct_collection_aggregates_emit_sorted_values() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d (x int, s text)").unwrap();
    e.execute("INSERT INTO d VALUES (2,'b'),(1,'a'),(2,'b'),(NULL,NULL)")
        .unwrap();
    for (sql, want) in [
        ("SELECT array_agg(DISTINCT x) FROM d", "{1,2,NULL}"),
        ("SELECT string_agg(DISTINCT s, ',') FROM d", "a,b"),
        ("SELECT json_agg(DISTINCT x) FROM d", "[1, 2, null]"),
        // An explicit ORDER BY still wins.
        (
            "SELECT array_agg(DISTINCT x ORDER BY x DESC) FROM d",
            "{NULL,2,1}",
        ),
        // Plain array_agg keeps input order; count is unaffected.
        ("SELECT array_agg(x) FROM d", "{2,1,2,NULL}"),
        ("SELECT count(DISTINCT x) FROM d", "2"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}

#[test]
fn the_array_function_core_is_unchanged() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT array_positions(ARRAY[10,20,30,20], 20)", "{2,4}"),
        ("SELECT array_remove(ARRAY[1,2,3,2], 2)", "{1,3}"),
        ("SELECT array_replace(ARRAY[1,2,3,2], 2, 9)", "{1,9,3,9}"),
        ("SELECT array_fill(7, ARRAY[3])", "{7,7,7}"),
        ("SELECT cardinality(ARRAY[[1,2],[3,4]])", "4"),
        ("SELECT array_length(ARRAY[[1,2],[3,4]], 2)", "2"),
        ("SELECT array_ndims(ARRAY[[1,2],[3,4]])", "2"),
        ("SELECT array_dims(ARRAY[[1,2],[3,4]])", "[1:2][1:2]"),
        ("SELECT ARRAY[1,2] || 3", "{1,2,3}"),
        ("SELECT 0 || ARRAY[1,2]", "{0,1,2}"),
        ("SELECT array_to_string(ARRAY[1,NULL,3], ',', 'X')", "1,X,3"),
        ("SELECT string_to_array('a,b,c', ',', 'b')", "{a,NULL,c}"),
        ("SELECT (ARRAY[10,20,30])[1:2]", "{10,20}"),
        ("SELECT (ARRAY[10,20,30])[5]", "NULL"),
        ("SELECT (ARRAY[[1,2],[3,4]])[2][1]", "3"),
        ("SELECT ARRAY[1,2] < ARRAY[1,2,3]", "t"),
        ("SELECT ARRAY[1,2,3] && ARRAY[3,4]", "t"),
        ("SELECT trim_array(ARRAY[1,2,3,4], 2)", "{1,2}"),
        ("SELECT array_position(ARRAY[1,NULL,2], NULL)", "2"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
    // Multidimensional search is refused, like PG.
    let got = err(&mut e, "SELECT array_position(ARRAY[[1,2],[3,4]], 1)");
    assert!(
        got.contains("searching for elements in multidimensional arrays is not supported"),
        "{got}"
    );
}
