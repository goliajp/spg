//! v7.39 (read01 utils/adt, round 21) — orderedsetaggs.c knives: the
//! percentile fraction contract (range error / NULL / NULL array
//! elements), the INTERVAL percentile_cont overload, multi-key
//! hypothetical-set aggregates — and the ORDER BY interval P0 the
//! probe surfaced (value_cmp fell to the debug-string fallback, so
//! interval sorts were ordered by the decimal micros rendering).
//! Byte-locked vs PG18.

use spg_engine::{Engine, QueryResult};

fn row_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(spg_engine::eval::value_to_text)
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn col_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err_of(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

#[test]
fn interval_order_by_is_span_ordered() {
    let mut e = Engine::new();
    // 4h ordered before 1h under the old debug-string fallback
    // ("14400000000" < "3600000000" lexicographically).
    assert_eq!(
        col_of(
            &mut e,
            "SELECT x FROM (VALUES (interval '4 hours'),(interval '1 hour'),\
             (interval '2 hours')) t(x) ORDER BY x"
        ),
        vec!["01:00:00", "02:00:00", "04:00:00"]
    );
    // A month counts as 30 days, a day as 24 hours (PG interval_cmp).
    assert_eq!(
        col_of(
            &mut e,
            "SELECT x FROM (VALUES (interval '1 month'),(interval '29 days'),\
             (interval '31 days')) t(x) ORDER BY x"
        ),
        vec!["29 days", "1 mon", "31 days"]
    );
}

#[test]
fn percentile_fraction_contract() {
    let mut e = Engine::new();
    assert!(
        err_of(
            &mut e,
            "SELECT percentile_cont(1.5) WITHIN GROUP (ORDER BY x) FROM (VALUES (1),(2)) t(x)"
        )
        .contains("percentile value 1.5 is not between 0 and 1")
    );
    assert!(
        err_of(
            &mut e,
            "SELECT percentile_cont(-0.1) WITHIN GROUP (ORDER BY x) FROM (VALUES (1),(2)) t(x)"
        )
        .contains("percentile value -0.1 is not between 0 and 1")
    );
    // NULL fraction → NULL result.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT percentile_cont(NULL::float8) WITHIN GROUP (ORDER BY x) \
             FROM (VALUES (1),(2)) t(x)"
        ),
        vec!["NULL"]
    );
    // A NULL array element yields a NULL result element (not 0).
    assert_eq!(
        row_of(
            &mut e,
            "SELECT percentile_disc(ARRAY[NULL,0.5]::float8[]) WITHIN GROUP (ORDER BY x) \
             FROM (VALUES (1),(2)) t(x)"
        ),
        vec!["{NULL,1}"]
    );
}

#[test]
fn percentile_cont_interval_overload() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY x) FROM \
             (VALUES (interval '1 hour'),(interval '2 hours'),(interval '4 hours')) t(x)"
        ),
        vec!["02:00:00"]
    );
    // Day remainder spills into the time field: 1d..3d at 0.25 = 1d 12h.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT percentile_cont(0.25) WITHIN GROUP (ORDER BY x) FROM \
             (VALUES (interval '1 day'),(interval '3 days')) t(x)"
        ),
        vec!["1 day 12:00:00"]
    );
}

#[test]
fn hypothetical_set_multi_key() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT rank(5, 'x') WITHIN GROUP (ORDER BY a, b) FROM \
             (VALUES (1,'a'),(5,'w'),(5,'y')) t(a,b)"
        ),
        vec!["3"]
    );
    // Direct-argument count must match the ordering-column count.
    // v7.39 (round 255) — this pin locked SPG's own wording; PG resolves
    // the mismatch as a missing overload whose signature is the direct
    // arguments followed by the WITHIN GROUP ones (probed live on this
    // exact query: `function rank(integer, integer, integer) does not
    // exist`).
    assert!(
        err_of(
            &mut e,
            "SELECT rank(2, 3) WITHIN GROUP (ORDER BY a) FROM (VALUES (1)) t(a)"
        )
        .contains("function rank(integer, integer, integer) does not exist")
    );
}
