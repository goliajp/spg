//! v7.39 (round 599) — a target-list SRF re-derived its whole plan for every
//! input row.
//!
//! `unnest` in the select list cost 6.9-8.6x PG18, and the shape said where
//! to look: the same 50k-row scan costs 2.67 ms plain, 47.16 ms with
//! `unnest(ARRAY[id])` over it, and 54.64 with a CONSTANT `unnest(ARRAY[1,2])`
//! — so the array's contents were irrelevant and round 597's constant-folding
//! would not have touched it. A counting allocator put the path at **24
//! allocations per input row** against 0 for the plain scan, 211 MB against
//! 4.3.
//!
//! `expand_srf_row` ran once per input row and, every time, cloned each
//! SRF-bearing projection expression, walked and rewrote the tree to lift the
//! SRF calls out, formatted a `__srf_N` name per lifted call, copied the whole
//! column schema, and rebuilt the extended schema. None of that depends on
//! the row. It is a plan now, built once per query; the row loop evaluates
//! the SRFs and the rewritten projection and nothing else. Per input row:
//!
//!     unnest(ARRAY[id])         24 -> 8 allocs   211.7 -> 32.6 MB
//!     unnest(ARRAY[id,g])       29 -> 11         259.7 -> 63.8
//!     generate_series(1,2)      27 -> 15         252.7 -> 72.9
//!
//! and over pgwire on 50k input rows, against PG18:
//!
//!     unnest(ARRAY[id])         47.16 -> 27.43 ms   PG 6.89   6.85x -> 3.98x
//!     unnest(ARRAY[id, g])      57.50 -> 34.55      PG 7.85   7.33x -> 4.40x
//!     unnest(ARRAY[id,g,id,g])  72.64 -> 46.48      PG 8.49   8.55x -> 5.47x
//!     unnest(ARRAY[1,2])        54.64 -> 33.78      PG 6.61   8.27x -> 5.11x
//!     generate_series(1,2)      54.68 -> 34.94      PG 7.06   7.74x -> 4.95x
//!
//! One behaviour moved, and toward PG. PG rejects a set-returning function
//! inside CASE or COALESCE at plan time, whatever the row count; SPG raised
//! it per row, so a query that matched NO rows returned empty instead of
//! erroring. The check lives in the plan now, so it fires either way — which
//! is the only difference between this round's output and the previous
//! binary's over the whole 18-shape differential.
//!
//! Thirteen of the eighteen shapes match live PG18 byte for byte and are
//! pinned below. The rest are pre-existing and go to the ledger, verified
//! against the previous binary: ORDER BY on an SRF's output alias raises
//! "function unnest(integer[]) does not exist", `SELECT DISTINCT <srf> …
//! ORDER BY 1` ignores the ordering — a wrong answer — and PG decorates its
//! CASE/COALESCE rejection with LINE and HINT lines that SPG does not emit.

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

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => alloc_string(&x),
        Ok(other) => panic!("{sql}: expected an error, got {other:?}"),
    }
}

fn alloc_string(e: &spg_engine::EngineError) -> String {
    format!("{e:?}")
}

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE sr (id INT, g INT, a INT[], s TEXT)").unwrap();
    e.execute(
        "INSERT INTO sr VALUES (1,10,ARRAY[1,2,3],'x'),(2,20,ARRAY[4],'y'),\
         (3,30,NULL,NULL),(4,40,ARRAY[]::INT[],'z')",
    )
    .unwrap();
    e
}

/// The plan is built once and reused, so every input row must still get its
/// own SRF values — a column array, a built array, and one per row of the
/// scan.
#[test]
fn round599_each_row_expands_with_its_own_values() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT id, unnest(a) FROM sr ORDER BY id, 2"),
        vec!["1|1", "1|2", "1|3", "2|4"],
        "a NULL array and an empty array both yield no rows"
    );
    assert_eq!(
        vals(&mut e, "SELECT id, unnest(ARRAY[id, g]) FROM sr ORDER BY id, 2"),
        vec!["1|1", "1|10", "2|2", "2|20", "3|3", "3|30", "4|4", "4|40"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, unnest(ARRAY[s, s]) FROM sr WHERE id = 1 ORDER BY 2"
        ),
        vec!["1|x", "1|x"],
        "a text SRF through the same plan"
    );
    assert_eq!(
        vals(&mut e, "SELECT id, unnest(a) FROM sr WHERE id >= 3 ORDER BY id"),
        Vec::<String>::new()
    );
    assert_eq!(
        vals(&mut e, "SELECT id, unnest(ARRAY[id]) FROM sr WHERE false"),
        Vec::<String>::new(),
        "no rows, no expansion"
    );
}

/// Two SRFs in one target list advance in lockstep and the shorter one pads
/// with NULL — the extended schema carries a slot for each, and the slots'
/// types are patched per row.
#[test]
fn round599_multiple_srfs_and_padding() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, unnest(ARRAY[1,2,3]), unnest(ARRAY[10,20]) FROM sr WHERE id = 1 ORDER BY 2"
        ),
        vec!["1|1|10", "1|2|20", "1|3|NULL"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, generate_series(1, id) FROM sr WHERE id < 4 ORDER BY id, 2"
        ),
        vec!["1|1", "2|1", "2|2", "3|1", "3|2", "3|3"],
        "the row's own value decides how many rows it yields"
    );
}

/// The SRF's value feeding an expression, and the plain columns beside it.
#[test]
fn round599_srf_inside_expressions() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, unnest(ARRAY[id, g]) + 100 FROM sr WHERE id = 2 ORDER BY 2"
        ),
        vec!["2|102", "2|120"]
    );
    assert_eq!(
        vals(&mut e, "SELECT id, unnest(ARRAY[id,g]) FROM sr ORDER BY id, 2 LIMIT 4"),
        vec!["1|1", "1|10", "2|2", "2|20"]
    );
    assert_eq!(
        vals(&mut e, "SELECT s.id, u FROM sr s, unnest(s.a) u ORDER BY s.id, u"),
        vec!["1|1", "1|2", "1|3", "2|4"],
        "the FROM-clause spelling takes a different path and must agree"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*), sum(v) FROM (SELECT unnest(ARRAY[id, g]) v FROM sr) q"
        ),
        vec!["8|110"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, u FROM (SELECT id, unnest(ARRAY[id,g]) u FROM sr) q WHERE u > 15 \
             ORDER BY id, u"
        ),
        vec!["2|20", "3|30", "4|40"]
    );
}

/// PG rejects a set-returning function inside a conditional at plan time,
/// whatever the row count. SPG raised it per row, so a query matching no
/// rows returned empty; the check is in the plan now and fires either way.
#[test]
fn round599_conditional_rejection_does_not_depend_on_rows() {
    let mut e = seed();
    assert!(
        err(&mut e, "SELECT CASE WHEN true THEN unnest(ARRAY[1,2]) END FROM sr")
            .contains("not allowed in CASE")
    );
    assert!(
        err(
            &mut e,
            "SELECT CASE WHEN true THEN unnest(ARRAY[1,2]) END FROM sr WHERE false"
        )
        .contains("not allowed in CASE"),
        "no rows is not a reason to accept it"
    );
    assert!(
        err(&mut e, "SELECT coalesce(unnest(ARRAY[1,2]), 0) FROM sr WHERE id = 1")
            .contains("not allowed in COALESCE")
    );
}

/// At a size where the per-row plan rebuild was the cost, the answer has to
/// be the one a plain scan gives.
#[test]
fn round599_scale_agrees_with_the_plain_scan() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT, g INT)").unwrap();
    e.execute("INSERT INTO big SELECT gg, gg % 7 FROM generate_series(1, 5000) gg")
        .unwrap();
    // Each row yields two, so the sum is the sum of both columns.
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*), sum(v) FROM (SELECT unnest(ARRAY[id, g]) v FROM big) q"
        ),
        vals(&mut e, "SELECT count(*) * 2, sum(id) + sum(g) FROM big")
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM (SELECT generate_series(1, 3) n FROM big) q"
        ),
        vec!["15000"]
    );
}
