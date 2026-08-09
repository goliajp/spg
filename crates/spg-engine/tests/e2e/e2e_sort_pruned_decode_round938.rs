//! r938 — the sort's output half decodes only the columns it will read.
//!
//! The sort spills the SOURCE row (round 836), so a narrow projection
//! used to rebuild every column of every row on the way out, including
//! a text payload nothing asked for. Skipping those is worth 8.3% on
//! `SELECT id FROM t400k ORDER BY k` (server-side, interleaved against
//! the same binary, control clean), and nothing at all on `SELECT pad
//! ... ORDER BY k`, where there is no column to skip — which is the
//! shape of a real saving rather than a measurement artefact.
//!
//! The risk it carries is the reason for this file. A column pruned by
//! mistake does not fail: it reads NULL. Nothing errors, nothing warns,
//! and the answer is simply wrong. So the mask is computed timidly
//! (every projection item a bare column, every ORDER BY key bound, or
//! else decode everything), and the cases below are the ones where a
//! mask that over-pruned would show.
//!
//! The spilled cases matter most. On the merge path the keys are
//! re-derived from the DECODED row, so an ORDER BY column that got
//! pruned would sort a column of NULLs — an answer in the wrong order
//! rather than an answer with a hole in it.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    Engine::new()
}

fn run(e: &mut Engine, sql: &str) -> Vec<String> {
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

/// 600 rows of a 200-byte payload, with `k` deliberately not equal to
/// `id` so an ordering mistake cannot pass by coincidence.
fn seeded(work_mem_kb: Option<u32>) -> Engine {
    let mut e = engine();
    if let Some(kb) = work_mem_kb {
        e.execute(&format!("SET work_mem = {kb}")).unwrap();
    }
    e.execute("CREATE TABLE t (id INT PRIMARY KEY, k INT, pad TEXT, note TEXT)")
        .unwrap();
    for id in 1..=600i32 {
        let k = (id * 7919) % 600;
        e.execute(&format!(
            "INSERT INTO t VALUES ({id}, {k}, '{}', 'n{id}')",
            "p".repeat(200)
        ))
        .unwrap();
    }
    e
}

fn expected_by_k() -> Vec<(i32, i32)> {
    let mut v: Vec<(i32, i32)> = (1..=600i32).map(|id| ((id * 7919) % 600, id)).collect();
    v.sort();
    v
}

/// The shape that prunes: `pad` and `note` are read by nobody.
#[test]
fn round938_a_projection_that_drops_the_text_still_answers_every_row() {
    for work_mem in [None, Some(64u32)] {
        let mut e = seeded(work_mem);
        let got = run(&mut e, "SELECT id FROM t ORDER BY k, id");
        let want: Vec<String> = expected_by_k()
            .iter()
            .map(|(_, id)| id.to_string())
            .collect();
        assert_eq!(got, want, "work_mem {work_mem:?}");
    }
}

/// The same query with the text projected: nothing may be pruned, and
/// the payload has to come back whole rather than as NULL.
#[test]
fn round938_a_projected_text_comes_back_whole() {
    for work_mem in [None, Some(64u32)] {
        let mut e = seeded(work_mem);
        let got = run(&mut e, "SELECT id, pad, note FROM t ORDER BY k, id");
        assert_eq!(got.len(), 600, "work_mem {work_mem:?}");
        let want = expected_by_k();
        for (i, row) in got.iter().enumerate() {
            let cells: Vec<&str> = row.split('|').collect();
            assert_eq!(cells[0], want[i].1.to_string(), "row {i} id");
            assert_eq!(cells[1].len(), 200, "row {i} pad truncated or NULL: {row}");
            assert_eq!(cells[2], format!("n{}", want[i].1), "row {i} note");
        }
    }
}

/// One text projected and one not — the mask has to be per column, not
/// per row. A mask that pruned both would show `pad` as NULL here.
#[test]
fn round938_one_text_kept_and_one_dropped() {
    for work_mem in [None, Some(64u32)] {
        let mut e = seeded(work_mem);
        let got = run(&mut e, "SELECT pad FROM t ORDER BY k, id");
        assert_eq!(got.len(), 600, "work_mem {work_mem:?}");
        assert!(
            got.iter().all(|r| r.len() == 200),
            "work_mem {work_mem:?}: a projected text came back short or NULL"
        );
    }
}

/// ORDER BY a text column that the projection drops. The key column has
/// to survive pruning or the merge re-keys from NULLs and the answer
/// comes back in the wrong order — the failure this file exists for.
#[test]
fn round938_ordering_by_a_text_the_projection_drops() {
    for work_mem in [None, Some(64u32)] {
        let mut e = seeded(work_mem);
        let got = run(&mut e, "SELECT id FROM t ORDER BY note, id");
        let mut want: Vec<(String, i32)> = (1..=600i32).map(|id| (format!("n{id}"), id)).collect();
        want.sort();
        let want: Vec<String> = want.iter().map(|(_, id)| id.to_string()).collect();
        assert_eq!(got, want, "work_mem {work_mem:?}");
    }
}

/// An expression in the select list is not a bare column, so the mask
/// declines and everything decodes. Pinned because the decline is what
/// keeps the timid rule timid.
#[test]
fn round938_an_expression_projection_still_reads_its_columns() {
    for work_mem in [None, Some(64u32)] {
        let mut e = seeded(work_mem);
        let got = run(&mut e, "SELECT upper(note) FROM t ORDER BY k, id");
        let want: Vec<String> = expected_by_k()
            .iter()
            .map(|(_, id)| format!("N{id}"))
            .collect();
        assert_eq!(got, want, "work_mem {work_mem:?}");
    }
}

/// `SELECT *` names every column without naming any, which the mask
/// must not read as "no column is needed".
#[test]
fn round938_select_star_keeps_every_column() {
    for work_mem in [None, Some(64u32)] {
        let mut e = seeded(work_mem);
        let got = run(&mut e, "SELECT * FROM t ORDER BY k, id");
        assert_eq!(got.len(), 600, "work_mem {work_mem:?}");
        for row in &got {
            let cells: Vec<&str> = row.split('|').collect();
            assert_eq!(cells.len(), 4, "arity: {row}");
            assert_eq!(cells[2].len(), 200, "pad came back short or NULL: {row}");
            assert!(cells[3].starts_with('n'), "note came back wrong: {row}");
        }
    }
}
