//! v7.39 (round 600) — an SRF query sorted by keys taken from the row it had
//! not expanded yet.
//!
//! Round 599's differential left two defects, both of them worse than
//! anything on the performance list. `ORDER BY` on a query whose select list
//! contains a set-returning function raised
//! `function unnest(integer[]) does not exist` whenever the key named the
//! SRF's own output — by alias, by repeating the call, or through
//! `ORDER BY 1` — and where it did not error it silently did nothing:
//! `SELECT DISTINCT unnest(…) FROM sr ORDER BY 1` came back in input order.
//!
//! One cause. The keys were built from the INPUT row, before the SRF
//! expanded, so a key naming the SRF was a scalar call to it. PG sorts AFTER
//! the expansion, and so does this now: a key that names a select-list item
//! reads that item's value out of the expanded row, and anything else — a
//! column the query does not project — is still evaluated against the input
//! row, which is the only place it exists.
//!
//! Three edits, and the middle one is why the first two were invisible:
//!
//!   * the keys are built per EXPANDED row, from the item's value;
//!   * the per-input-row build that ran BEFORE the SRF branch is skipped for
//!     an SRF query. It was still evaluating the ORDER BY against the input
//!     row and throwing the error, which is why building better keys further
//!     down changed nothing at all until this went with it;
//!   * an ordinal resolves to the output column. `resolve_positional_order_by`
//!     deliberately refuses to expand an ordinal that points at a
//!     set-returning item — copying the call into ORDER BY would have made
//!     the key "the whole set" back when keys came from the input row — and
//!     reading the expanded row's Nth column is what it should have meant.
//!
//! All seven ORDER BY shapes here, and all eighteen of round 599's, now match
//! live PG18.

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
    e.execute("CREATE TABLE sr (id INT, g INT, a INT[], s TEXT)").unwrap();
    e.execute(
        "INSERT INTO sr VALUES (1,10,ARRAY[1,2,3],'x'),(2,20,ARRAY[4],'y'),\
         (3,30,NULL,NULL),(4,40,ARRAY[]::INT[],'z')",
    )
    .unwrap();
    e
}

/// Every way of naming the SRF's own output.
#[test]
fn round600_order_by_the_srf_output() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[g, id]) v FROM sr WHERE id < 3 ORDER BY v DESC"
        ),
        vec!["20", "10", "2", "1"],
        "by alias"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[id, g]) FROM sr WHERE id < 3 ORDER BY 1"
        ),
        vec!["1", "2", "10", "20"],
        "by ordinal"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[g, id]) FROM sr WHERE id < 3 ORDER BY unnest(ARRAY[g, id])"
        ),
        vec!["1", "2", "10", "20"],
        "by repeating the call, which names the same select-list item"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[g, id]) v FROM sr ORDER BY v LIMIT 3"
        ),
        vec!["1", "2", "3"],
        "and the LIMIT is applied to the sorted expansion"
    );
}

/// Mixed keys: an input column and the SRF's output in one ORDER BY, and a
/// key that names a column the query does not project.
#[test]
fn round600_mixed_and_input_only_keys() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, unnest(ARRAY[id,g]) u FROM sr WHERE id < 3 ORDER BY id, u"
        ),
        vec!["1|1", "1|10", "2|2", "2|20"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, unnest(ARRAY[id,g]) FROM sr WHERE id < 3 ORDER BY id DESC"
        ),
        vec!["2|2", "2|20", "1|1", "1|10"],
        "ordering by a projected input column"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[id,g]) v FROM sr WHERE id < 3 ORDER BY g DESC, v"
        ),
        vec!["2", "20", "1", "10"],
        "a key naming a column the query does NOT project stays on the input row"
    );
}

/// DISTINCT sorts what survives the dedup, and the ordering used to be
/// dropped entirely.
#[test]
fn round600_distinct_over_an_srf_is_ordered() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT DISTINCT unnest(ARRAY[id % 2, g % 2]) FROM sr ORDER BY 1"
        ),
        vec!["0", "1"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT DISTINCT unnest(ARRAY[g, id]) v FROM sr WHERE id < 3 ORDER BY v DESC"
        ),
        vec!["20", "10", "2", "1"]
    );
}

/// NULLs and their placement come from the same packer every other ORDER BY
/// uses, so the explicit spellings have to behave.
#[test]
fn round600_null_placement() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[s, NULL]) v FROM sr WHERE id < 3 ORDER BY v"
        ),
        vec!["x", "y", "NULL", "NULL"],
        "NULLs last by default, ascending"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[s, NULL]) v FROM sr WHERE id < 3 ORDER BY v NULLS FIRST"
        ),
        vec!["NULL", "NULL", "x", "y"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT unnest(ARRAY[s, NULL]) v FROM sr WHERE id < 3 ORDER BY v DESC"
        ),
        vec!["NULL", "NULL", "y", "x"],
        "NULLs first descending"
    );
}

/// At a size where the ordering is not accidental.
#[test]
fn round600_scale() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT, g INT)").unwrap();
    e.execute("INSERT INTO big SELECT gg, 10000 - gg FROM generate_series(1, 2000) gg")
        .unwrap();
    let out = vals(
        &mut e,
        "SELECT unnest(ARRAY[id, g]) v FROM big ORDER BY v LIMIT 5",
    );
    assert_eq!(out, vec!["1", "2", "3", "4", "5"]);
    let out = vals(
        &mut e,
        "SELECT unnest(ARRAY[id, g]) v FROM big ORDER BY v DESC LIMIT 3",
    );
    assert_eq!(out, vec!["9999", "9998", "9997"]);
    // Every value appears exactly once across the two columns here, so the
    // sorted expansion is a permutation of 1..2000 plus 8000..9999.
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*), count(DISTINCT v) FROM (SELECT unnest(ARRAY[id, g]) v FROM big) q"
        ),
        vec!["4000|4000"]
    );
}
