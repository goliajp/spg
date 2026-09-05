//! v7.40.4 — a large sort splits across threads, and the answer does not
//! change.
//!
//! The release panel's remaining sort losses were not a comparator
//! defect. Measured on the panel's own fixtures at 400,000 rows, server
//! reported: every cell SPG lost or tied was a cell where PostgreSQL
//! launched parallel workers, and every cell PostgreSQL ran on one
//! process SPG won by 0.67x-0.79x. Seven out of seven. SPG had no
//! parallel execution at all — `max_parallel_workers_per_gather` was in
//! the GUC catalogue and nothing read it.
//!
//! What makes splitting safe here is that every comparator the prefix
//! sort uses ends on the row index, so no two distinct elements compare
//! equal: the order is strict and total, and a total order has exactly
//! one sorted arrangement. Sorting the pieces and merging them cannot
//! reach a different answer than sorting the whole.
//!
//! "Cannot" is the claim, so this asks for it: the same query with the
//! workers off and on, row for row. The table is above
//! `parsort::MIN_PARALLEL`, because below it the module returns the
//! serial path and a pin built on a small table would pass without ever
//! reaching the code it names.
//!
//! ONE test over ONE fixture, and every shape asserted inside it. This
//! step builds in debug, where a 66,000-row table is not free, and the
//! algorithm itself is already pinned exhaustively and cheaply by the
//! unit tests in `parsort` — eight lengths across six worker settings,
//! including a non-`Copy` key, in 0.02 s. What is left for this file is
//! the WIRING: that the engine reaches the module at all, on both of
//! its sort paths, and that the GUC a customer sets is the one that
//! decides.

use spg_engine::{Engine, QueryResult};

/// Above `parsort::MIN_PARALLEL` (65,536) so the split actually happens,
/// and no further: every row past it is debug-profile time spent proving
/// something the unit tests already prove.
const N: i64 = 66_000;

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows")
    };
    rows.iter()
        .map(|r| {
            r.values
                .iter()
                .map(spg_engine::eval::value_to_text)
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

/// `g*7919 mod N` is a permutation of `0..N`, so no sort sees its input
/// in order, and every column is built from it so all the shapes
/// disagree with insertion order the same way. One statement rather than
/// sixty-six thousand.
fn seeded() -> Engine {
    let mut e = Engine::new();
    // `ci` carries a collation whose order is not byte order, which is
    // what sends the sort down its OTHER path -- the one that builds an
    // ICU sort key per row. That path is what a customer on a locale
    // database actually runs, and a pin that only covered the byte path
    // would have left it on one thread with nothing saying so.
    e.execute(
        r#"CREATE TABLE t (id bigint, k bigint, s text, dup text, ci text COLLATE "case_insensitive")"#,
    )
    .unwrap();
    e.execute(&format!(
        "INSERT INTO t SELECT g, (g*7919)%{N}, \
         'k' || lpad(((g*7919)%{N})::text, 8, '0'), \
         chr(97 + (((g*7919)%{N}) % 26)::int), \
         CASE WHEN g % 2 = 0 THEN chr(65 + (((g*7919)%{N}) % 26)::int) \
              ELSE chr(97 + (((g*7919)%{N}) % 26)::int) END \
         FROM generate_series(1,{N}) g"
    ))
    .unwrap();
    e
}

#[test]
fn the_split_does_not_change_any_answer() {
    let mut e = seeded();
    // Each shape reaches a different branch of the sort:
    //
    //   s          an EXACT key -- every value inside sixteen bytes, so
    //              a tie means the values are equal and the comparator
    //              never touches a row. A pure integer sort.
    //   k          the same branch through the integer door.
    //   dup, k     two terms, so a tie on the first is settled by the
    //              second and the merge runs under a comparator that
    //              READS ROWS. Twenty-six distinct values in the first
    //              term means every merge step meets ties.
    //   k DESC     the split reverses the comparator and not the merge;
    //              a merge that assumed ascending would interleave the
    //              runs backwards.
    //   ci         the COLLATED path, which does not build a prefix key
    //              at all: an ICU sort key per row, a `Vec<u8>`, and a
    //              tie broken by reading the rows. Twenty-six letters in
    //              two cases over sixty-six thousand rows means the
    //              merge meets ties everywhere.
    let shapes = [
        "SELECT s FROM t ORDER BY s",
        "SELECT k FROM t ORDER BY k",
        "SELECT dup, k FROM t ORDER BY dup, k",
        "SELECT k FROM t ORDER BY k DESC",
        "SELECT ci FROM t ORDER BY ci",
        "SELECT ci, id FROM t ORDER BY ci DESC, id",
    ];
    for sql in shapes {
        e.execute("SET max_parallel_workers_per_gather = 0")
            .unwrap();
        let serial = rows(&mut e, sql);
        assert_eq!(
            serial.len(),
            usize::try_from(N).unwrap(),
            "{sql}: the fixture must be large enough to reach the parallel path"
        );
        // Every setting a customer might have inherited from their
        // PostgreSQL configuration, including more workers than this
        // machine has cores.
        for setting in ["1", "2", "3", "7", "64"] {
            e.execute(&format!("SET max_parallel_workers_per_gather = {setting}"))
                .unwrap();
            assert_eq!(
                serial,
                rows(&mut e, sql),
                "{sql}: max_parallel_workers_per_gather = {setting} changed the answer"
            );
        }
        // Reproducible is not the same as right: a merge that dropped
        // the same rows every time would satisfy the loop above. For the
        // two shapes whose order this file can compute for itself, say
        // so. It transfers to the split, because the rows are equal to
        // it.
        //
        // Asserted HERE rather than in a second test. A draft had one,
        // and under the process-wide thread lease it could take no
        // threads at all -- the other test was holding them -- sort
        // serially, and pass while proving nothing. The ablation caught
        // it: breaking the merge reddened this test and left that one
        // green.
        if sql == "SELECT s FROM t ORDER BY s" {
            let mut want = serial.clone();
            want.sort();
            assert_eq!(serial, want, "ascending text is not in order");
        }
        if sql == "SELECT k FROM t ORDER BY k DESC" {
            let got: Vec<i64> = serial
                .iter()
                .map(|x| x.parse().expect("k is an integer"))
                .collect();
            let mut want = got.clone();
            want.sort_unstable_by(|a, b| b.cmp(a));
            assert_eq!(got, want, "descending order is wrong");
        }
    }
}
