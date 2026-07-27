//! v7.39 (round 555) — V-2 verified: neither spill nor OOM.
//!
//! The audit's phase 6 is "verification debt — not undone, unproven",
//! and V-2 asks whether exceeding `work_mem` spills rather than
//! OOM-ing. Measured on the testbed against PG18, over a 300k-row table
//! with `work_mem = '64kB'`:
//!
//!     sort / GROUP BY / DISTINCT   both engines return the same rows
//!     repeated full sorts          SPG residency steady at 576 MB
//!     table doubled                579 -> 640 MB, sort peak 820 MB
//!
//! So the answer is NEITHER: SPG does not OOM, does not leak across
//! repeats, and does not spill — `work_mem` is parsed, validated,
//! stored, echoed by SHOW, and never consulted. A DBA setting it to
//! bound memory gets no bound and no error.
//!
//! One inference this round made from EXPLAIN was WRONG and measuring
//! corrected it. `ORDER BY … LIMIT 5` showed a Seq Scan with
//! `actual rows=600000` feeding a Sort, which read as a full sort of
//! 600k rows for five results. Residency says otherwise: the LIMIT 5
//! sort added 0 MB where the same sort without a LIMIT added 28. The
//! streaming top-N trim was doing its job — a scan reads every row
//! either way, so the row count could never have told them apart.
//!
//! What that left was a real gap of the round-551 kind: the bound was
//! in force and INVISIBLE. The node said only "Sort", so a reader could
//! not tell it from the unbounded one — and EXPLAIN is where they would
//! look. It names the choice now, as PG names its own.
//!
//! No Memory figure beside it: PG measures its sort's peak and SPG does
//! not meter one, and a number that was not measured is worse than
//! none.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
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

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE w555 (id INT, k INT, pad TEXT)").unwrap();
    e.execute("INSERT INTO w555 SELECT g, g % 100, repeat('x', 20) FROM generate_series(1, 5000) g")
        .unwrap();
    e
}

/// A tiny work_mem changes no answer — SPG completes rather than
/// failing, which is the half of V-2 that matters most.
#[test]
fn round555_tiny_work_mem_changes_no_answer() {
    let mut e = engine();
    e.execute("SET work_mem = '64kB'").unwrap();
    assert_eq!(rows(&mut e, "SELECT current_setting('work_mem')"), vec!["64kB"]);
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM (SELECT id FROM w555 ORDER BY pad, id) s"),
        vec!["5000"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT count(*) FROM (SELECT k, count(*) FROM w555 GROUP BY k) g"
        ),
        vec!["100"]
    );
    assert_eq!(
        rows(&mut e, "SELECT count(*) FROM (SELECT DISTINCT k, pad FROM w555) d"),
        vec!["100"]
    );
    // The first five by the sort key, with the bound in force.
    assert_eq!(
        rows(&mut e, "SELECT id FROM w555 ORDER BY pad, id LIMIT 3"),
        vec!["1", "2", "3"]
    );
}

/// EXPLAIN names which sort runs, so the O(k) bound is visible.
#[test]
fn round555_explain_names_the_sort_method() {
    let mut e = engine();
    let plan = |e: &mut Engine, sql: &str| -> Vec<String> {
        match e.execute(sql).unwrap() {
            QueryResult::Rows { rows, .. } => rows
                .iter()
                .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
                .collect(),
            other => panic!("{other:?}"),
        }
    };
    // PG prints this under ANALYZE and not under a plain EXPLAIN — it is
    // a measured runtime fact, not a plan property. A plain EXPLAIN must
    // stay exactly as PG's is.
    let plain = plan(&mut e, "EXPLAIN SELECT id FROM w555 ORDER BY pad, id LIMIT 5");
    assert!(
        !plain.iter().any(|l| l.contains("Sort Method")),
        "a plain EXPLAIN has no Sort Method line in PG: {plain:?}"
    );
    let bounded = plan(
        &mut e,
        "EXPLAIN ANALYZE SELECT id FROM w555 ORDER BY pad, id LIMIT 5",
    );
    assert!(
        bounded
            .iter()
            .any(|l| l.contains("Sort Method: top-N heapsort")),
        "{bounded:?}"
    );
    let full = plan(&mut e, "EXPLAIN ANALYZE SELECT id FROM w555 ORDER BY pad, id");
    assert!(
        full.iter().any(|l| l.contains("Sort Method: quicksort")),
        "{full:?}"
    );
    // No invented Memory figure — SPG does not meter one.
    assert!(
        !bounded.iter().any(|l| l.contains("Memory:")),
        "a number that was not measured is worse than none: {bounded:?}"
    );
}

/// work_mem round-trips, and a bad value is still refused as PG refuses it.
#[test]
fn round555_work_mem_still_validates() {
    let mut e = Engine::new();
    e.execute("SET work_mem = '8MB'").unwrap();
    assert_eq!(rows(&mut e, "SELECT current_setting('work_mem')"), vec!["8MB"]);
    assert!(e.execute("SET work_mem = 'bogus'").is_err());
}
