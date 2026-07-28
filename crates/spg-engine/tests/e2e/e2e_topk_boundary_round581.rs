//! v7.39 (round 581) — a row that cannot reach the answer is no longer
//! built.
//!
//! Round 580 left `ORDER BY … LIMIT` at 2.07x for one key and 4.00x for
//! two. The two-key figure is the tell: PG answers
//! `ORDER BY g DESC, id DESC LIMIT 10` FASTER than the single-key form
//! (7.4 ms against 10.4), because with 50 distinct `g` nearly every row
//! is decided on the first key and costs it one comparison. SPG built
//! both keys AND the projected row for all 500,000 before discarding
//! them.
//!
//! After a trim the accumulator holds the best `keep` rows seen, so the
//! worst of them is a floor on the answer: the final k-th best can only
//! be better. A row that LOSES to that floor can never enter the top k,
//! and the keys — which have to be built to compare — are enough to
//! know. The projection is skipped.
//!
//! The check earns its place only on rows it rejects, and over ascending
//! ids `ORDER BY id DESC` rejects none: every row beats the current
//! worst. Measured, that cost +5.5% in three batches of three. So after
//! a window of 8192 checks it looks at what it actually rejected and
//! switches itself off below a quarter. With that, all four shapes
//! improve — engine-side, 500k rows:
//!
//!     ORDER BY g DESC, id DESC LIMIT 10   28.64 -> 19.41 ms   -32%
//!     ORDER BY id LIMIT 10                21.89 -> 12.17      -44%
//!     ORDER BY id DESC LIMIT 10           20.86 -> 20.08      -3.7%
//!     ORDER BY id DESC LIMIT 1000         24.53 -> 23.86      -2.7%
//!
//! Over pgwire in a warm session against PG18:
//!
//!     two keys, LIMIT 10   29.73 -> 20.60   PG 7.05   4.22x -> 2.92x
//!     ORDER BY id LIMIT 10 22.58 -> 13.29   PG 6.56   3.44x -> 2.03x
//!     ORDER BY id DESC L10 21.71 -> 21.61   PG 10.70          2.02x
//!
//! The rule that makes this safe is that only a STRICTLY worse row is
//! dropped. A row equal to the floor has to be kept, or a tie at the
//! k-th position would lose rows — that is what the first pin below is
//! for. The gate changes only how often the question is asked, never the
//! answer, so the pins run shapes whose rejection rate is 0%, 100%, and
//! one that changes partway through the scan.

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

/// Ties AT the boundary must survive — dropping an equal row would lose
/// output whenever the k-th place is shared.
#[test]
fn round581_ties_at_the_boundary_are_kept() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE b581 (id INT, k INT)").unwrap();
    // Every k value appears 100 times, so any LIMIT lands mid-tie.
    e.execute("INSERT INTO b581 SELECT gg, gg % 200 FROM generate_series(1, 20000) gg")
        .unwrap();
    // The 10 largest k are all 199 — there are 100 of them.
    let got = vals(&mut e, "SELECT k FROM b581 ORDER BY k DESC LIMIT 10");
    assert_eq!(got.len(), 10);
    assert!(got.iter().all(|v| v == "199"), "{got:?}");
    // Asking for more than the tie's width crosses into the next value.
    let got = vals(&mut e, "SELECT k FROM b581 ORDER BY k DESC LIMIT 150");
    assert_eq!(got.iter().filter(|v| *v == "199").count(), 100);
    assert_eq!(got.iter().filter(|v| *v == "198").count(), 50);
    // A second key breaks the tie and the answer is exact.
    assert_eq!(
        vals(&mut e, "SELECT id, k FROM b581 ORDER BY k DESC, id DESC LIMIT 3"),
        vec!["19999|199", "19799|199", "19599|199"]
    );
}

/// The three rejection regimes: never, always, and changing partway.
#[test]
fn round581_every_rejection_regime_answers_the_same() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE r581 (id INT, asc_v INT, desc_v INT, half INT)")
        .unwrap();
    // asc_v rises with storage order (so ORDER BY asc_v DESC never
    // rejects); desc_v falls (so ORDER BY desc_v DESC rejects almost
    // everything after the first rows); half is flat for the first 15000
    // rows and then rises, so the rejection rate changes long after the
    // 8192-check window has closed.
    e.execute(
        "INSERT INTO r581 SELECT gg, gg, 30000 - gg, \
         CASE WHEN gg <= 15000 THEN 0 ELSE gg END FROM generate_series(1, 30000) gg",
    )
    .unwrap();
    assert_eq!(
        vals(&mut e, "SELECT asc_v FROM r581 ORDER BY asc_v DESC LIMIT 3"),
        vec!["30000", "29999", "29998"],
        "nothing is ever rejected"
    );
    assert_eq!(
        vals(&mut e, "SELECT desc_v FROM r581 ORDER BY desc_v DESC LIMIT 3"),
        vec!["29999", "29998", "29997"],
        "almost everything is rejected"
    );
    assert_eq!(
        vals(&mut e, "SELECT half FROM r581 ORDER BY half DESC LIMIT 3"),
        vec!["30000", "29999", "29998"],
        "the regime changes long after the window closed"
    );
    // And ascending, where the winners all arrive first.
    assert_eq!(
        vals(&mut e, "SELECT asc_v FROM r581 ORDER BY asc_v LIMIT 3"),
        vec!["1", "2", "3"]
    );
    assert_eq!(
        vals(&mut e, "SELECT half FROM r581 ORDER BY half LIMIT 3"),
        vec!["0", "0", "0"]
    );
}

/// NULLs sit at one end or the other depending on the direction, and a
/// NULL boundary must compare the same way the sort does.
#[test]
fn round581_null_boundaries() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE n581 (id INT, v INT)").unwrap();
    e.execute(
        "INSERT INTO n581 SELECT gg, CASE WHEN gg % 3 = 0 THEN NULL ELSE gg END \
         FROM generate_series(1, 30000) gg",
    )
    .unwrap();
    assert_eq!(
        vals(&mut e, "SELECT v FROM n581 ORDER BY v LIMIT 3"),
        vec!["1", "2", "4"],
        "NULLs last ascending"
    );
    let desc = vals(&mut e, "SELECT v FROM n581 ORDER BY v DESC LIMIT 3");
    assert!(desc.iter().all(|x| x == "NULL"), "NULLs first descending: {desc:?}");
    assert_eq!(
        vals(&mut e, "SELECT v FROM n581 ORDER BY v DESC NULLS LAST LIMIT 3"),
        vec!["29999", "29998", "29996"]
    );
    assert_eq!(
        vals(&mut e, "SELECT v FROM n581 ORDER BY v NULLS FIRST LIMIT 3")
            .iter()
            .filter(|x| **x == "NULL")
            .count(),
        3
    );
}

/// OFFSET widens what has to be kept, so the floor has to be the
/// (limit + offset)-th row, not the limit-th.
#[test]
fn round581_offset_widens_the_floor() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE o581 (id INT)").unwrap();
    e.execute("INSERT INTO o581 SELECT gg FROM generate_series(1, 30000) gg")
        .unwrap();
    assert_eq!(
        vals(&mut e, "SELECT id FROM o581 ORDER BY id DESC LIMIT 3 OFFSET 5000"),
        vec!["25000", "24999", "24998"]
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM o581 ORDER BY id LIMIT 3 OFFSET 5000"),
        vec!["5001", "5002", "5003"]
    );
}
