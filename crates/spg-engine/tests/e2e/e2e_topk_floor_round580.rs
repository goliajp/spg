//! v7.39 (round 580) — the top-N accumulator trimmed fifty thousand
//! times to keep ten rows.
//!
//! Round 579 re-measured the read panel in a WARM session and found that
//! most of the losses this session had been chasing were cold-start
//! artefacts. Two survived: the filtered join at 1.58x and
//! `ORDER BY … LIMIT` at 2.47x. This is the second.
//!
//! Round 571 pooled the sort's per-row buffers, and a counting allocator
//! confirms that worked — `ORDER BY id DESC LIMIT 1` over 500k rows now
//! makes 48 allocations in total. But it still costs 25.8 ms where
//! `max(id)`, the same answer over the same scan, costs 7.3. Profiling
//! it, the largest single symbol is the COMPARATOR at 8.6%.
//!
//! `topk_trim` fired whenever the accumulator reached `2 * keep`. For
//! `LIMIT 10` over 500k rows that is fifty thousand trims, each a
//! `select_nth_unstable_by` over twenty elements plus a drain and a
//! return to the buffer pool. Selecting the same ten out of a larger
//! batch costs the same O(n) per element but pays the call overhead a
//! hundredth as often, so the trigger has a floor now. The accumulator
//! stays bounded — by 1024 rows rather than by `2k` — which is the cost
//! of the change and is why the floor is not larger.
//!
//! Engine-side, 500k rows, 6-second loops:
//!
//!     ORDER BY id DESC LIMIT 10      26.52 -> 20.83 ms   -21%
//!     ORDER BY g DESC, id DESC L10   35.00 -> 28.57      -18%
//!     ORDER BY id LIMIT 10           22.49 -> 21.88      -2.7%
//!     ORDER BY id DESC LIMIT 1000    24.48 -> 24.47      unchanged
//!
//! The last row is the shape of the change: `2k` already exceeds the
//! floor at `LIMIT 1000`, so nothing moves. Over pgwire in a warm
//! session against PG18:
//!
//!     ORDER BY id DESC LIMIT 10      27.58 -> 21.47   PG 10.37   2.66x -> 2.07x
//!     ORDER BY g DESC, id DESC L10   36.18 -> 29.66   PG  7.42   4.88x -> 4.00x
//!
//! PG is still ahead. What the pins below check is that changing WHEN
//! the accumulator trims changes nothing about WHAT it keeps, across k
//! on both sides of the floor.

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

fn engine(n: i32) -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE f580 (id INT, g INT, t TEXT)")
        .unwrap();
    e.execute(&format!(
        "INSERT INTO f580 SELECT gg, gg % 13, 'r' || gg FROM generate_series(1, {n}) gg"
    ))
    .unwrap();
    e
}

/// k on both sides of the 1024 floor, and either side of `2k` crossing
/// it — the answers must not depend on when the accumulator trims.
#[test]
fn round580_every_k_keeps_the_same_rows() {
    let mut e = engine(3000);
    for k in [1usize, 2, 9, 10, 511, 512, 513, 1023, 1024, 1025, 2000] {
        let got = vals(
            &mut e,
            &format!("SELECT id FROM f580 ORDER BY id DESC LIMIT {k}"),
        );
        let want: Vec<String> = (3000 - k as i32 + 1..=3000)
            .rev()
            .map(|i| i.to_string())
            .collect();
        assert_eq!(got, want, "LIMIT {k} descending");
        let got = vals(
            &mut e,
            &format!("SELECT id FROM f580 ORDER BY id LIMIT {k}"),
        );
        let want: Vec<String> = (1..=k as i32).map(|i| i.to_string()).collect();
        assert_eq!(got, want, "LIMIT {k} ascending");
    }
}

/// A table SMALLER than the floor never trims at all — the ordinary
/// sort has to give the same answer.
#[test]
fn round580_small_inputs_never_trim() {
    let mut e = engine(40);
    assert_eq!(
        vals(&mut e, "SELECT id FROM f580 ORDER BY id DESC LIMIT 3"),
        vec!["40", "39", "38"]
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM f580 ORDER BY id DESC LIMIT 100"),
        (1..=40)
            .rev()
            .map(|i: i32| i.to_string())
            .collect::<Vec<_>>(),
        "a LIMIT beyond the input returns all of it"
    );
    let mut e = Engine::new();
    e.execute("CREATE TABLE z580 (id INT)").unwrap();
    assert!(vals(&mut e, "SELECT id FROM z580 ORDER BY id DESC LIMIT 5").is_empty());
}

/// Ties, multiple keys and OFFSET all cross the floor the same way.
#[test]
fn round580_ties_keys_and_offset() {
    let mut e = engine(3000);
    // g repeats every 13 rows, so the leading key ties heavily and the
    // second key decides.
    let got = vals(
        &mut e,
        "SELECT id, g FROM f580 ORDER BY g DESC, id DESC LIMIT 4",
    );
    let want: Vec<String> = (1..=3000)
        .rev()
        .filter(|i: &i32| i % 13 == 12)
        .take(4)
        .map(|i| format!("{i}|12"))
        .collect();
    assert_eq!(got, want);
    // OFFSET counts toward what has to be kept.
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM f580 ORDER BY id DESC LIMIT 3 OFFSET 1020"
        ),
        vec!["1980", "1979", "1978"],
        "the offset reaches past the floor"
    );
    // NULLs in the key, both directions.
    let mut e = Engine::new();
    e.execute("CREATE TABLE n580 (id INT, v INT)").unwrap();
    e.execute("INSERT INTO n580 SELECT gg, CASE WHEN gg % 4 = 0 THEN NULL ELSE gg END FROM generate_series(1, 2000) gg")
        .unwrap();
    assert_eq!(
        vals(&mut e, "SELECT v FROM n580 ORDER BY v LIMIT 3"),
        vec!["1", "2", "3"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT v FROM n580 ORDER BY v DESC NULLS LAST LIMIT 3"
        ),
        vec!["1999", "1998", "1997"]
    );
}

/// The shapes that never stream keep their own path and their own
/// answers.
#[test]
fn round580_non_streaming_shapes_unchanged() {
    let mut e = engine(3000);
    assert_eq!(
        vals(
            &mut e,
            "SELECT DISTINCT g FROM f580 ORDER BY g DESC LIMIT 3"
        ),
        vec!["12", "11", "10"]
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM f580 ORDER BY id DESC").len(),
        3000,
        "no LIMIT means no trimming"
    );
    let ties = (1..=3000).filter(|i: &i32| i % 13 == 12).count();
    assert_eq!(
        vals(
            &mut e,
            "SELECT g FROM f580 ORDER BY g DESC FETCH FIRST 1 ROW WITH TIES"
        )
        .len(),
        ties,
        "every row whose g is the maximum comes back"
    );
}
