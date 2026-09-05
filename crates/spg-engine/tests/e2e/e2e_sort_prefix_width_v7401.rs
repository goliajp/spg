//! v7.40.1 — the sort's prefix key takes its width from the data, and
//! both widths must answer what `str` answers.
//!
//! The prefix key was eight bytes for every text column. The release
//! panel priced that against PostgreSQL 18.6, in memory on both legs,
//! 400,000 rows:
//!
//! ```text
//!   short text (9 bytes, shared prefix)    SPG 96.3   PG 72.7   1.36x behind
//!   long text (192 bytes, byte 0 decides)  SPG 85.6   PG 80.4   parity
//! ```
//!
//! `'k' || lpad(n, 8, '0')` is nine bytes, so eight bytes drop the last
//! digit: ten rows share every key and forty thousand tie-runs fall back
//! to the full comparator, each reading at random into a 400,000-element
//! array. Widened to sixteen and measured in the same window, three
//! binaries named by md5, order digests identical to PostgreSQL's:
//!
//! ```text
//!   short text   96.3 -> 53.4 ms   0.73x of PG18, from 1.36x behind
//!   long text    85.6 -> 81.4 ms   parity, unchanged
//! ```
//!
//! These rows pin the ANSWER on both sides of the width choice, because
//! the choice is what could break it:
//!
//!   * every value at sixteen bytes or under makes the wide key the
//!     WHOLE key, and the code sets `exact`, which skips the tie
//!     fallback entirely
//!   * values longer than that keep the narrow key, where a tie decides
//!     nothing and the full comparator must still run
//!
//! The second case is the one with teeth. Keys that agree for sixteen
//! bytes and differ at the seventeenth tie on ANY prefix; if `exact`
//! were ever set for them, the sort would return them in input order and
//! this file would say so.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows")
    };
    rows.iter()
        .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
        .collect()
}

/// Nine bytes each, sharing `k` and two zeros: inside the wide key,
/// outside the narrow one. Inserted in an order no sort would produce.
#[test]
fn short_keys_that_share_a_prefix_come_back_in_order() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE s (v text)").unwrap();
    e.execute("INSERT INTO s VALUES ('k00000042'),('k00000007'),('k00000199'),('k00000008')")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT v FROM s ORDER BY v"),
        ["k00000007", "k00000008", "k00000042", "k00000199"]
    );
    assert_eq!(
        rows(&mut e, "SELECT v FROM s ORDER BY v DESC"),
        ["k00000199", "k00000042", "k00000008", "k00000007"]
    );
}

/// Seventeen bytes, identical for sixteen of them. No prefix of any
/// width decides these, so the full comparator has to.
#[test]
fn keys_that_differ_only_past_the_wide_prefix_still_sort() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE s (v text)").unwrap();
    e.execute(
        "INSERT INTO s VALUES ('aaaaaaaaaaaaaaaad'),('aaaaaaaaaaaaaaaab'),\
         ('aaaaaaaaaaaaaaaac'),('aaaaaaaaaaaaaaaaa')",
    )
    .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT v FROM s ORDER BY v"),
        [
            "aaaaaaaaaaaaaaaaa",
            "aaaaaaaaaaaaaaaab",
            "aaaaaaaaaaaaaaaac",
            "aaaaaaaaaaaaaaaad"
        ]
    );
}

/// A value SHORTER than another that it is a prefix of. Zero-padding the
/// key makes `ab` and `ab\0…` the same sixteen bytes, so the shorter one
/// has to win on length, which only the full comparison knows.
#[test]
fn a_prefix_sorts_before_the_string_it_is_a_prefix_of() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE s (v text)").unwrap();
    e.execute("INSERT INTO s VALUES ('abc'),('ab'),('abcd'),('a')")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT v FROM s ORDER BY v"),
        ["a", "ab", "abc", "abcd"]
    );
}

/// Mixed lengths across the boundary: the batch has a value over sixteen
/// bytes, so the whole batch takes the narrow key, and the short ones
/// must still land correctly among the long ones.
#[test]
fn a_batch_that_straddles_the_boundary_sorts_as_one() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE s (v text)").unwrap();
    e.execute(
        "INSERT INTO s VALUES ('zz'),('aaaaaaaaaaaaaaaaaaaaaaaa'),('m'),\
         ('aaaaaaaaaaaaaaaaaaaaaaab'),('aa')",
    )
    .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT v FROM s ORDER BY v"),
        [
            "aa",
            "aaaaaaaaaaaaaaaaaaaaaaaa",
            "aaaaaaaaaaaaaaaaaaaaaaab",
            "m",
            "zz"
        ]
    );
}

/// v7.40.2 — the low-cardinality run walk, which reads a compact column
/// rather than the rows.
///
/// When the prefix key does not discriminate, the sort orders the keys
/// and then proves per run whether every value in it is equal; a run
/// that is uniform is already in stable order and is left alone. That
/// proof used to walk `order -> tagged -> values -> Value -> &str ->
/// bytes` for every row, the first hop scattered across the whole
/// batch. It reads a `Vec<&str>` collected in one sequential pass now,
/// built only when the run walk will actually happen.
///
/// Measured, 400,000 rows, twenty-six distinct values, server-reported,
/// three md5-distinct binaries in one window, order digests identical
/// to PostgreSQL's:
///
/// ```text
///   value length   head    compact   compact (only when low_card)
///   8 bytes       57.78     58.62         56.85
///   200 bytes     94.51     81.21         81.16
/// ```
///
/// The middle column is why the build is conditional: an exact key
/// never calls the uniform check, so collecting the column for it was
/// 0.8 ms charged to a shape that does not read it.
///
/// This pins the ANSWER for the shape that takes the path: few distinct
/// values, each longer than any prefix, with rows interleaved so input
/// order is not the answer.
#[test]
fn a_low_cardinality_text_sort_orders_its_runs() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE lc (id int, v text)").unwrap();
    let mut sql = String::from("INSERT INTO lc VALUES ");
    for g in 0..120u32 {
        if g > 0 {
            sql.push(',');
        }
        // Three distinct values, each 40 characters, round-robin: every
        // eight-byte prefix inside one letter is identical, so the runs
        // are what decide.
        let ch = char::from(b'a' + u8::try_from(g % 3).expect("0..3"));
        sql.push_str(&format!("({g}, '{}')", ch.to_string().repeat(40)));
    }
    e.execute(&sql).unwrap();
    let got = rows(&mut e, "SELECT v FROM lc ORDER BY v LIMIT 3");
    assert_eq!(got, ["a".repeat(40), "a".repeat(40), "a".repeat(40)]);
    let tail = rows(&mut e, "SELECT v FROM lc ORDER BY v DESC LIMIT 2");
    assert_eq!(tail, ["c".repeat(40), "c".repeat(40)]);
    // Every row comes back, and the three runs are whole: forty of each
    // value in order. A compact column indexed wrongly would interleave
    // them, which counting per value catches and a LIMIT would not.
    assert_eq!(
        rows(
            &mut e,
            "SELECT count(*) FROM (SELECT v FROM lc ORDER BY v) z"
        ),
        ["120"]
    );
    let all = rows(&mut e, "SELECT v FROM lc ORDER BY v");
    assert_eq!(all.len(), 120);
    for (i, v) in all.iter().enumerate() {
        let want = char::from(b'a' + u8::try_from(i / 40).expect("0..3"))
            .to_string()
            .repeat(40);
        assert_eq!(*v, want, "row {i}");
    }
    // What is deliberately NOT asserted: the order of rows that tie.
    // A draft pinned the ids inside one run to input order and it read
    // `60, 6, 9, 12`. Neither this engine nor PostgreSQL promises an
    // order between equal keys, and a pin that demands one is pinning
    // the implementation rather than the answer.
}
