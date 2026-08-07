//! v7.39 (round 621) — an ORDER BY over a set-returning projection threw most
//! of the answer away.
//!
//! `SELECT unnest(ARRAY[1,2]), y FROM unnest(ARRAY[5,6,7]) y ORDER BY 1`
//! answered three of its six rows, in no order at all. Without the ORDER BY
//! the same query was already correct — which is what makes this the kind of
//! wrong answer worth hunting: adding a sort to a working query silently
//! dropped half the rows.
//!
//! The sort built its keys from the INPUT rows and then indexed the EXPANDED
//! rows by the input row's position. One line:
//!
//!     projected_rows = indexed.into_iter().map(|(i, _)| projected_rows[i].clone())
//!
//! With no SRF that is exactly right — one output row per input row, the
//! indices line up. With one, `projected_rows` is longer than `filtered`, so
//! the result was truncated to the input row count and paired by the wrong
//! index at the same time.
//!
//! Keys are now built per OUTPUT row. Which row a key is read from is the
//! question round 600 already answered for the other SRF paths: a key that
//! names a select-list item reads it out of the expanded row, because PG sorts
//! AFTER the expansion; a key that names a source column the query does not
//! project is evaluated against the input row that output row came from, which
//! is why each output row remembers where it came from.
//!
//! An earlier pass at this round mis-read the same symptom as a bug in the
//! expansion itself and wrote it up that way. Re-measuring without the ORDER BY
//! showed the expansion had been right all along — six rows, correctly paired.
//! The clause was the whole of it.
//!
//! All shapes below were checked against live PG18 and match byte for byte.

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

/// The clause that dropped the rows.
#[test]
fn round621_order_by_sorts_the_expanded_rows() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[1,2]), y FROM unnest(ARRAY[5,6,7]) y ORDER BY 1"
        ),
        vec!["1|5", "1|6", "1|7", "2|5", "2|6", "2|7"],
        "six rows, sorted by the SRF's own column — three, unsorted, before"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[1,2]), y FROM unnest(ARRAY[5,6,7]) y ORDER BY 2"
        ),
        vec!["1|5", "2|5", "1|6", "2|6", "1|7", "2|7"],
        "and by the source column"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[1,2]), y FROM unnest(ARRAY[5,6,7]) y ORDER BY 1,2"
        ),
        vec!["1|5", "1|6", "1|7", "2|5", "2|6", "2|7"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[1,2]), y FROM unnest(ARRAY[5,6,7]) y ORDER BY y"
        ),
        vec!["1|5", "2|5", "1|6", "2|6", "1|7", "2|7"],
        "a key named rather than positional"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[1,2]), y FROM unnest(ARRAY[5,6,7]) y ORDER BY 1 DESC"
        ),
        vec!["2|5", "2|6", "2|7", "1|5", "1|6", "1|7"]
    );
}

/// ORDER BY beside the clauses that come after it.
#[test]
fn round621_with_limit_offset_and_distinct() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[1,2]), y FROM unnest(ARRAY[5,6,7]) y ORDER BY 1 LIMIT 4"
        ),
        vec!["1|5", "1|6", "1|7", "2|5"],
        "LIMIT takes the first four OF THE SORTED SIX, not of a truncated three"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[1,2]), y FROM unnest(ARRAY[5,6,7]) y ORDER BY 1 OFFSET 4"
        ),
        vec!["2|6", "2|7"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT DISTINCT unnest(ARRAY[1,1,2]) FROM unnest(ARRAY[5,6]) y ORDER BY 1"
        ),
        vec!["1", "2"]
    );
}

/// The shapes that were already right and must stay so.
#[test]
fn round621_what_was_already_right() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[1,2]), y FROM unnest(ARRAY[5,6,7]) y"
        ),
        vec!["1|5", "2|5", "1|6", "2|6", "1|7", "2|7"],
        "no ORDER BY — this was correct before and is the reason the \
         expansion was ruled out"
    );
    assert_eq!(
        vals(&mut e, "SELECT y FROM unnest(ARRAY[7,5,6]) y ORDER BY 1"),
        vec!["5", "6", "7"],
        "ORDER BY with no SRF in the projection: the indices line up and \
         always did"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[1,2]), y FROM unnest(ARRAY[5,6,7]) y LIMIT 4"
        ),
        vec!["1|5", "2|5", "1|6", "2|6"],
        "LIMIT without ORDER BY"
    );
    let mut e2 = Engine::new();
    e2.execute("CREATE TABLE u2 (x INT)").unwrap();
    e2.execute("INSERT INTO u2 VALUES (4),(3)").unwrap();
    assert_eq!(
        vals(&mut e2, "SELECT unnest(ARRAY[1,2]), x FROM u2 ORDER BY 2,1"),
        vec!["1|3", "2|3", "1|4", "2|4"],
        "a real table under the same shape — a different execution path, and \
         one that already sorted the expanded rows"
    );
}
