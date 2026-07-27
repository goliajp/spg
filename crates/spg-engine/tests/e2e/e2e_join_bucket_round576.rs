//! v7.39 (round 576) — the hash join allocated once per peer row.
//!
//! Round 575 found the allocator at 28% of a self-join and could not
//! name the caller from a frame-pointer profile. A profile was the wrong
//! instrument: `alloc_count_probe` installs a counting
//! `#[global_allocator]` and reads it around each query, which answers
//! the question without needing a symbol. On 200k rows a side:
//!
//!     single scan, count       47 allocations
//!     join, no predicate  200,127            1.00 per row
//!     join, peer 100      200,155            1.00
//!     join, both 100      200,259            1.00
//!
//! One allocation for every peer row, and the same 200,000 of them
//! whether the query wanted 200,000 rows or 100. The build side's hash
//! is `HashMap<key, Vec<usize>>`, and an FK-to-PK join is unique on that
//! side — so nearly every bucket was a `Vec` holding exactly one index,
//! each with its own heap allocation. The comment there already knew the
//! shape ("the bucket is a one-element Vec") and had halved the cost by
//! pre-sizing to 1; it still allocated.
//!
//! `Bucket::One` holds that index inline and promotes to a `Vec` on the
//! second row. After:
//!
//!     join, no predicate      127 allocations   (was 200,127)
//!     join, peer 100          155               (was 200,155)
//!     join, both 100          259               (was 200,259)
//!     join, both 20k       20,175               one per matched pair
//!
//! Engine-side that is 15.6 -> 8.6 ms with no predicate, 11.0 -> 7.4
//! with both sides cut to 100. Over pgwire on 500k rows a side, three
//! paired batches:
//!
//!     both sides 100   65.02 -> 56.91 ms   3 of 3   PG18 12.3
//!     left side 20k    64.46 -> 58.05      3 of 3   PG18 18.4
//!     no predicate     76.09 -> 72.37      2 of 3   PG18 59.7
//!
//! The wire dilutes it — a one-row answer still costs ~57 ms there
//! against ~18 ms of engine work at that size — but the allocation
//! itself is gone.
//!
//! What the pins are for: a bucket that forgets its second row would
//! silently drop join output, and only a duplicated join key shows it.

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

fn one(e: &mut Engine, sql: &str) -> String {
    vals(e, sql).first().cloned().unwrap_or_default()
}

/// An integer key with duplicates on the build side: the bucket has to
/// promote and keep every row.
#[test]
fn round576_duplicate_int_keys_keep_every_row() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE l576 (id INT, k INT)").unwrap();
    e.execute("CREATE TABLE r576 (id INT, k INT)").unwrap();
    e.execute("INSERT INTO l576 VALUES (1, 7), (2, 8), (3, 9)")
        .unwrap();
    // Three rows share k = 7, two share k = 8, one has k = 9.
    e.execute("INSERT INTO r576 VALUES (10,7),(11,7),(12,7),(13,8),(14,8),(15,9)")
        .unwrap();
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM l576 a JOIN r576 b ON a.k = b.k"),
        "6",
        "3 + 2 + 1 pairs"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT b.id FROM l576 a JOIN r576 b ON a.k = b.k WHERE a.k = 7 ORDER BY b.id"
        ),
        vec!["10", "11", "12"],
        "the promoted bucket keeps all three"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT b.id FROM l576 a JOIN r576 b ON a.k = b.k WHERE a.k = 9 ORDER BY b.id"
        ),
        vec!["15"],
        "the inline bucket keeps its one"
    );
}

/// The same for the string-keyed table, which is the other bucket map.
#[test]
fn round576_duplicate_text_keys_keep_every_row() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE lt576 (id INT, k TEXT)").unwrap();
    e.execute("CREATE TABLE rt576 (id INT, k TEXT)").unwrap();
    e.execute("INSERT INTO lt576 VALUES (1,'a'), (2,'b')").unwrap();
    e.execute("INSERT INTO rt576 VALUES (10,'a'),(11,'a'),(12,'a'),(13,'b')")
        .unwrap();
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM lt576 a JOIN rt576 b ON a.k = b.k"),
        "4"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT b.id FROM lt576 a JOIN rt576 b ON a.k = b.k WHERE a.k = 'a' ORDER BY b.id"
        ),
        vec!["10", "11", "12"]
    );
    // A two-column key takes the same map.
    e.execute("CREATE TABLE l2576 (a INT, b TEXT)").unwrap();
    e.execute("CREATE TABLE r2576 (a INT, b TEXT, id INT)").unwrap();
    e.execute("INSERT INTO l2576 VALUES (1,'x')").unwrap();
    e.execute("INSERT INTO r2576 VALUES (1,'x',100),(1,'x',101),(1,'y',102)")
        .unwrap();
    assert_eq!(
        vals(
            &mut e,
            "SELECT r.id FROM l2576 l JOIN r2576 r ON l.a = r.a AND l.b = r.b ORDER BY r.id"
        ),
        vec!["100", "101"]
    );
}

/// A NULL join key matches nothing on either side, and the bucket must
/// not be asked for one.
#[test]
fn round576_null_keys_match_nothing() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ln576 (id INT, k INT)").unwrap();
    e.execute("CREATE TABLE rn576 (id INT, k INT)").unwrap();
    e.execute("INSERT INTO ln576 VALUES (1, NULL), (2, 5)").unwrap();
    e.execute("INSERT INTO rn576 VALUES (10, NULL), (11, NULL), (12, 5)")
        .unwrap();
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM ln576 a JOIN rn576 b ON a.k = b.k"),
        "1",
        "only the 5 = 5 pair"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM ln576 a LEFT JOIN rn576 b ON a.k = b.k"
        ),
        "2",
        "the NULL-keyed left row survives, unmatched"
    );
}

/// Outer joins track which build rows matched, so a promoted bucket must
/// not lose that either.
#[test]
fn round576_outer_joins_see_every_build_row() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE lo576 (id INT, k INT)").unwrap();
    e.execute("CREATE TABLE ro576 (id INT, k INT)").unwrap();
    e.execute("INSERT INTO lo576 VALUES (1, 7)").unwrap();
    e.execute("INSERT INTO ro576 VALUES (10,7),(11,7),(12,99)")
        .unwrap();
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM lo576 a RIGHT JOIN ro576 b ON a.k = b.k"),
        "3",
        "two matches plus the unmatched build row"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT b.id FROM lo576 a RIGHT JOIN ro576 b ON a.k = b.k \
             WHERE a.id IS NULL ORDER BY b.id"
        ),
        vec!["12"]
    );
}

/// At a size that crosses the map's own growth, with the answer checked
/// against the row counts rather than a hand-written list.
#[test]
fn round576_scale_with_duplicates() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ls576 (id INT, k INT)").unwrap();
    e.execute("CREATE TABLE rs576 (id INT, k INT)").unwrap();
    e.execute("INSERT INTO ls576 SELECT gg, gg % 500 FROM generate_series(1, 2000) gg")
        .unwrap();
    e.execute("INSERT INTO rs576 SELECT gg, gg % 500 FROM generate_series(1, 3000) gg")
        .unwrap();
    // 500 distinct keys; left has 4 rows each, right has 6 → 500*4*6.
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM ls576 a JOIN rs576 b ON a.k = b.k"),
        "12000"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM ls576 a JOIN rs576 b ON a.k = b.k WHERE a.k = 3"
        ),
        "24"
    );
}
