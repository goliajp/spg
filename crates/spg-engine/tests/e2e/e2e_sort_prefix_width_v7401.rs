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
