//! v7.39 (round 592) — the window path built a throwaway row per input row,
//! and `*` handed the client the internal column it built it with.
//!
//! Round 589's candidate list needed re-measuring before anything could be
//! picked from it: two of its entries had been read off a server still
//! recovering from a 36-second query. Warm and clean, `id % 3 = 0` is 2.9x,
//! not the 30.8x recorded, and the real leader is `lag()` at 18.7x —
//! 119.23 ms against PG18's 6.38 over 500k rows.
//!
//! A counting allocator (round 576's instrument) named the cost without a
//! profile. Per input row, 200k rows:
//!
//!     plain derived table                        1.00 allocations
//!     lag(id) OVER ()                            4.00
//!     lag(id) OVER (ORDER BY id)                 5.00
//!     lag(id) OVER (PARTITION BY g)              5.00
//!     lag(id) OVER (PARTITION BY g ORDER BY id)  6.00
//!
//! Three of those are unconditional: the input row, then an extended row
//! built by cloning the input values into a fresh Vec and growing it once to
//! take the window columns, then the projected row. Each key column adds its
//! own Vec on top. Only the projected row has to exist afterwards, so the
//! extended row is now one buffer refilled per row:
//!
//!     OVER ()                            4.00 -> 2.00   28.6 -> 24.1 ms
//!     OVER (ORDER BY id)                 5.00 -> 3.00   39.8 -> 33.3
//!     OVER (PARTITION BY g)              5.00 -> 3.00   69.1 -> 61.6
//!     OVER (PARTITION BY g ORDER BY id)  6.00 -> 4.00  116.3 -> 108.6
//!
//! Over pgwire on 500k rows the `lag()` shape goes 119.23 -> 89.66 ms, PG
//! 6.38: 18.7x -> 14.1x. Still a loss. The per-row key Vec and the sort are
//! what remain, and naming their share wants a profile this round did not
//! manage to get (see the ledger).
//!
//! The differential written to check the reused buffer found something else
//! — present before this round's change, and worse than what it was sent to
//! fix. The synthetic `__win_N` column that carries each window's computed
//! values was visible to `*`:
//!
//!     SELECT * FROM (SELECT wr.*, row_number() OVER (ORDER BY id) rn
//!                    FROM wr) q
//!
//! answered SIX columns where PG answers five, the sixth being the internal
//! one's value repeated. Wrong output, and silent. Those columns are
//! appended last, so they are hidden from `*` by position — by name would
//! look safe until a real column carried the name, which is what round 512
//! recorded about the system columns.

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

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE wr (id INT, p INT, v INT, s TEXT)").unwrap();
    e.execute(
        "INSERT INTO wr SELECT gg, gg % 4, CASE WHEN gg % 5 = 0 THEN NULL ELSE gg END, \
         CASE WHEN gg % 3 = 0 THEN NULL ELSE 'r' || gg END FROM generate_series(1, 2000) gg",
    )
    .unwrap();
    e
}

/// `*` must not see the column the window path invented to carry its own
/// values.
#[test]
fn round592_wildcards_do_not_expand_window_columns() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT * FROM (SELECT wr.*, row_number() OVER (ORDER BY id) rn FROM wr) q \
             WHERE id <= 3 ORDER BY id"
        ),
        vec!["1|1|1|r1|1", "2|2|2|r2|2", "3|3|3|NULL|3"],
        "four table columns and rn, not five and a repeat"
    );
    // A bare `*` alongside the window call, and two windows at once.
    assert_eq!(
        vals(
            &mut e,
            "SELECT * FROM (SELECT *, row_number() OVER (ORDER BY id) rn, \
             lag(id) OVER (ORDER BY id) lg FROM wr) q WHERE id <= 2 ORDER BY id"
        ),
        vec!["1|1|1|r1|1|NULL", "2|2|2|r2|2|1"]
    );
    // `*` with no window function at all is untouched.
    assert_eq!(
        vals(&mut e, "SELECT * FROM wr WHERE id = 1"),
        vec!["1|1|1|r1"]
    );
    // A window whose value is never projected still must not leak.
    assert_eq!(
        vals(
            &mut e,
            "SELECT * FROM (SELECT wr.* FROM wr ORDER BY row_number() OVER (ORDER BY id)) q \
             WHERE id <= 2 ORDER BY id"
        ),
        vec!["1|1|1|r1", "2|2|2|r2"]
    );
}

/// The extended row is one reused buffer now, so a stale value left between
/// rows would be a silent wrong answer that only a long run shows. Several
/// windows at once, over NULLs and text.
#[test]
fn round592_reused_row_buffer_keeps_every_row_its_own() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, lag(id) OVER (ORDER BY id), lead(id) OVER (ORDER BY id), \
             row_number() OVER (PARTITION BY p ORDER BY id), sum(v) OVER (PARTITION BY p) \
             FROM wr WHERE id <= 12 ORDER BY id"
        ),
        vec![
            "1|NULL|2|1|10",
            "2|1|3|1|8",
            "3|2|4|1|21",
            "4|3|5|1|24",
            "5|4|6|2|10",
            "6|5|7|2|8",
            "7|6|8|2|21",
            "8|7|9|2|24",
            "9|8|10|3|10",
            "10|9|11|3|8",
            "11|10|12|3|21",
            "12|11|NULL|3|24",
        ]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, lag(s) OVER (ORDER BY id), max(s) OVER (PARTITION BY p) \
             FROM wr WHERE id <= 10 ORDER BY id"
        ),
        vec![
            "1|NULL|r5",
            "2|r1|r2",
            "3|r2|r7",
            "4|NULL|r8",
            "5|r4|r5",
            "6|r5|r2",
            "7|NULL|r7",
            "8|r7|r8",
            "9|r8|r5",
            "10|NULL|r2",
        ],
        "text and NULLs through the same buffer"
    );
    // Offsets and defaults at the far end of the input.
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, lag(id, 2, -1) OVER (ORDER BY id), lead(id, 3, -9) OVER (ORDER BY id) \
             FROM wr WHERE id >= 1996 ORDER BY id"
        ),
        vec![
            "1996|-1|1999",
            "1997|-1|2000",
            "1998|1996|-9",
            "1999|1997|-9",
            "2000|1998|-9",
        ]
    );
}

/// Over the whole 2000 rows, checked by sums rather than by listing them —
/// a buffer that carried a value forward would move these.
#[test]
fn round592_whole_input_checksums() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT sum(id), sum(rn), count(l), count(s2) FROM \
             (SELECT id, row_number() OVER (ORDER BY id) rn, lag(id) OVER (ORDER BY id) l, \
              lag(s) OVER (ORDER BY id) s2 FROM wr) q"
        ),
        vec!["2001000|2001000|1999|1333"],
        "row_number sums to the same as id; lag loses one; lag(s) loses the NULLs too"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, row_number() OVER (ORDER BY id) FROM wr ORDER BY id OFFSET 1997 LIMIT 3"
        ),
        vec!["1998|1998", "1999|1999", "2000|2000"]
    );
}

/// The window value is read back by the projection, the outer ORDER BY and
/// DISTINCT — all three go through the same buffer.
#[test]
fn round592_window_values_feed_the_rest_of_the_query() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, sum(v) OVER (PARTITION BY p) t FROM wr WHERE id <= 20 \
             ORDER BY t DESC, id LIMIT 6"
        ),
        vec!["1|40", "2|40", "3|40", "4|40", "5|40", "6|40"],
        "every partition happens to sum to 40, so the tiebreak is id"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM (SELECT DISTINCT p, sum(v) OVER (PARTITION BY p) FROM wr) q"
        ),
        vec!["4"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, sum(v) OVER (PARTITION BY p) * 2 + id FROM wr WHERE id <= 8 ORDER BY id"
        ),
        vec!["1|3", "2|18", "3|23", "4|28", "5|7", "6|22", "7|27", "8|32"],
        "the window value inside an expression"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM (SELECT id, row_number() OVER (PARTITION BY p ORDER BY id) rn \
             FROM wr) q WHERE rn <= 3"
        ),
        vec!["12"]
    );
}

/// A window function may appear in ORDER BY without being selected. Only the
/// select list was consulted when deciding whether a statement needs the
/// window pass, so these took the ordinary path and the call reached row
/// eval — which answered the client with the internal message
/// "window function reached row eval — engine rewrite bug". PG answers them.
#[test]
fn round592_order_by_a_window_that_is_not_selected() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM wr ORDER BY row_number() OVER (ORDER BY id DESC) LIMIT 4"
        ),
        vec!["2000", "1999", "1998", "1997"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM wr WHERE id <= 8 ORDER BY sum(v) OVER (PARTITION BY p) DESC, id \
             LIMIT 5"
        ),
        vec!["4", "8", "3", "7", "2"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, lag(id) OVER (ORDER BY id) FROM wr WHERE id <= 5 \
             ORDER BY row_number() OVER (ORDER BY id DESC)"
        ),
        vec!["5|4", "4|3", "3|2", "2|1", "1|NULL"],
        "one window selected, a different one ordering"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT * FROM wr WHERE id <= 3 ORDER BY row_number() OVER (ORDER BY id DESC)"
        ),
        vec!["3|3|3|NULL", "2|2|2|r2", "1|1|1|r1"],
        "and `*` still shows only the table's own columns"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM wr ORDER BY row_number() OVER (ORDER BY id DESC) OFFSET 1998 LIMIT 2"
        ),
        vec!["2", "1"]
    );
}
