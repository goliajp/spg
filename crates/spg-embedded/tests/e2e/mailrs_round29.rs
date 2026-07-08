//! mailrs embed round-29 — aggregate `FILTER (WHERE …)` clause.
//! Standard SQL since SQL:2003 (T612) / PG core since 9.4. Reported
//! as a hard parse error that a consumer swallowed into wrong numbers
//! ("0 unread" on full mailboxes). Note:
//! `.claude/notes/mailrs-embed-round29-aggregate-filter-clause-unsupported.md`.
//!
//! FILTER is implemented as a first-class per-aggregate row predicate
//! (not desugared to `agg(CASE WHEN p THEN arg END)` — that rewrite is
//! faithful for NULL-ignoring aggregates but silently WRONG for
//! `array_agg`, which would collect a NULL per excluded row). The
//! `array_agg_filter_excludes_rather_than_nulls` test pins exactly that.

use spg_embedded::{Database, QueryResult, Value};

fn rows_of(db: &mut Database, sql: &str) -> Vec<Vec<Value<'static>>> {
    match db.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        other => panic!("expected rows for {sql}, got {other:?}"),
    }
}

fn seed(db: &mut Database) {
    db.execute("CREATE TABLE m (id BIGINT, flags INT, thread_id TEXT)")
        .unwrap();
    // flags & 1 == 0  → "read/seen"; the customer's unread counter is
    // COUNT(*) FILTER (WHERE (flags & 1) = 0).
    db.execute("INSERT INTO m VALUES (1,0,'t1'),(2,1,'t1'),(3,0,'t2'),(4,1,'t2'),(5,0,'t2')")
        .unwrap();
}

/// The customer's exact repro: `COUNT(*) FILTER (WHERE …)` in the
/// select list under a GROUP BY.
#[test]
fn count_star_filter_in_select_list() {
    let mut db = Database::open_in_memory();
    seed(&mut db);
    let r = rows_of(
        &mut db,
        "SELECT thread_id, COUNT(*) FILTER (WHERE (flags & 1) = 0) AS seen \
         FROM m GROUP BY thread_id ORDER BY thread_id",
    );
    assert_eq!(r[0], vec![Value::text("t1"), Value::BigInt(1)]);
    assert_eq!(r[1], vec![Value::text("t2"), Value::BigInt(2)]);
}

/// Several aggregates with independent filters in one grouped pass,
/// alongside an unfiltered aggregate.
#[test]
fn multiple_independent_filters_one_pass() {
    let mut db = Database::open_in_memory();
    seed(&mut db);
    let r = rows_of(
        &mut db,
        "SELECT thread_id, COUNT(*) AS total, \
                COUNT(*) FILTER (WHERE flags = 0) AS seen, \
                SUM(id) FILTER (WHERE flags = 1) AS unseen_ids \
         FROM m GROUP BY thread_id ORDER BY thread_id",
    );
    // t1: total 2, seen 1, sum(id where flags=1) = 2
    assert_eq!(
        r[0],
        vec![
            Value::text("t1"),
            Value::BigInt(2),
            Value::BigInt(1),
            Value::Numeric { scaled: 2, scale: 0 },
        ]
    );
    // t2: total 3, seen 2, sum(id where flags=1) = 4
    assert_eq!(
        r[1],
        vec![
            Value::text("t2"),
            Value::BigInt(3),
            Value::BigInt(2),
            Value::Numeric { scaled: 4, scale: 0 },
        ]
    );
}

/// The customer's HAVING shape: `... HAVING COUNT(*) FILTER (WHERE …)`.
#[test]
fn filter_in_having() {
    let mut db = Database::open_in_memory();
    seed(&mut db);
    let r = rows_of(
        &mut db,
        "SELECT thread_id FROM m GROUP BY thread_id \
         HAVING COUNT(*) FILTER (WHERE (flags & 1) = 0) > 1 ORDER BY thread_id",
    );
    // t1 has 1 seen, t2 has 2 — only t2 passes.
    assert_eq!(r, vec![vec![Value::text("t2")]]);
}

/// Ungrouped (whole-table) filtered aggregate.
#[test]
fn filter_without_group_by() {
    let mut db = Database::open_in_memory();
    seed(&mut db);
    let r = rows_of(&mut db, "SELECT COUNT(*) FILTER (WHERE flags = 0) FROM m");
    assert_eq!(r[0][0], Value::BigInt(3)); // ids 1,3,5
}

/// FILTER that excludes every row: COUNT → 0, SUM → NULL (PG: an
/// aggregate over no rows is its empty value, not an error).
#[test]
fn filter_excludes_all_rows() {
    let mut db = Database::open_in_memory();
    seed(&mut db);
    let r = rows_of(
        &mut db,
        "SELECT COUNT(*) FILTER (WHERE flags = 99), SUM(id) FILTER (WHERE flags = 99) FROM m",
    );
    assert_eq!(r[0][0], Value::BigInt(0));
    assert_eq!(r[0][1], Value::Null);
}

/// The correctness case the `CASE WHEN` desugar gets WRONG:
/// `array_agg` collects NULLs, so `array_agg(CASE WHEN p THEN id END)`
/// would emit a NULL element for every excluded row. The first-class
/// filter must instead EXCLUDE those rows entirely.
#[test]
fn array_agg_filter_excludes_rather_than_nulls() {
    let mut db = Database::open_in_memory();
    seed(&mut db);
    let r = rows_of(
        &mut db,
        "SELECT thread_id, array_agg(id) FILTER (WHERE flags = 0) AS a \
         FROM m GROUP BY thread_id ORDER BY thread_id",
    );
    // t1: only id 1 has flags=0 — NOT [1, NULL].
    assert_eq!(r[0][1], Value::BigIntArray(vec![Some(1)]));
    // t2: ids 3 and 5 — NOT [3, NULL, 5].
    assert_eq!(r[1][1], Value::BigIntArray(vec![Some(3), Some(5)]));
}

/// FILTER composes with DISTINCT: dedupe applies only to the rows the
/// filter admits.
#[test]
fn filter_composes_with_distinct() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE e (g TEXT, v INT, keep BOOLEAN)")
        .unwrap();
    db.execute(
        "INSERT INTO e VALUES \
         ('g',1,true),('g',1,true),('g',2,true),('g',2,false),('g',3,false)",
    )
    .unwrap();
    // DISTINCT v among kept rows = {1,2} → 2.
    let r = rows_of(
        &mut db,
        "SELECT COUNT(DISTINCT v) FILTER (WHERE keep) FROM e GROUP BY g",
    );
    assert_eq!(r[0][0], Value::BigInt(2));
}

/// A FILTER-bearing aggregate round-trips through the statement's
/// Display (used by the plan cache and the WAL bind-final renderer),
/// so re-parsing the rendered SQL must produce the same answer.
#[test]
fn filter_round_trips_through_display() {
    let mut db = Database::open_in_memory();
    seed(&mut db);
    // string_agg over the rendered-then-reparsed shape: the engine
    // renders prepared statements back to SQL for the WAL; if FILTER
    // dropped on the way out the second execution would diverge.
    let sql = "SELECT thread_id, COUNT(*) FILTER (WHERE flags = 1) AS unseen \
               FROM m GROUP BY thread_id ORDER BY thread_id";
    let first = rows_of(&mut db, sql);
    let again = rows_of(&mut db, sql);
    assert_eq!(first, again);
    assert_eq!(first[0][1], Value::BigInt(1)); // t1: one flags=1 row
    assert_eq!(first[1][1], Value::BigInt(1)); // t2: one flags=1 row
}

// ---------------------------------------------------------------------
// round-29 swept the whole aggregate-completeness class, not just the
// reported FILTER point: the rest of the standard aggregate-call grammar
// (WITHIN GROUP ordered-set aggregates) and the common analytics
// function inventory (statistical + bitwise) an ORM/BI tool emits.
// ---------------------------------------------------------------------

// v7.38 (read01, T4) — statistical / SUM aggregates over integers now return
// NUMERIC (matching PG); compare by value, tolerant of Float vs Numeric.
fn num_f64(v: &Value) -> f64 {
    match v {
        Value::Float(x) => *x,
        Value::Numeric { scaled, scale } => *scaled as f64 / 10f64.powi(i32::from(*scale)),
        Value::Int(n) => f64::from(*n),
        Value::BigInt(n) => *n as f64,
        other => panic!("not a numeric value: {other:?}"),
    }
}

fn seed_nums(db: &mut Database) {
    db.execute("CREATE TABLE t (g TEXT, x INT)").unwrap();
    // a = {1,2,3,4}; b = {10,10,20}
    db.execute("INSERT INTO t VALUES ('a',1),('a',2),('a',3),('a',4),('b',10),('b',10),('b',20)")
        .unwrap();
}

/// `percentile_cont(f) WITHIN GROUP (ORDER BY x)` — interpolated.
#[test]
fn percentile_cont_within_group() {
    let mut db = Database::open_in_memory();
    seed_nums(&mut db);
    let r = rows_of(
        &mut db,
        "SELECT g, percentile_cont(0.5) WITHIN GROUP (ORDER BY x) \
         FROM t GROUP BY g ORDER BY g",
    );
    assert_eq!(r[0][1], Value::Float(2.5)); // median of 1,2,3,4
    assert_eq!(r[1][1], Value::Float(10.0)); // median of 10,10,20
    // whole-table quartile: sorted 1,2,3,4,10,10,20 ; rank 0.25*6=1.5 → 2.5
    let r = rows_of(
        &mut db,
        "SELECT percentile_cont(0.25) WITHIN GROUP (ORDER BY x) FROM t",
    );
    assert_eq!(r[0][0], Value::Float(2.5));
}

/// `percentile_disc(f)` picks an actual member; `mode()` the most
/// frequent (ties → smallest).
#[test]
fn percentile_disc_and_mode_within_group() {
    let mut db = Database::open_in_memory();
    seed_nums(&mut db);
    let r = rows_of(
        &mut db,
        "SELECT g, percentile_disc(0.5) WITHIN GROUP (ORDER BY x), \
                mode() WITHIN GROUP (ORDER BY x) \
         FROM t GROUP BY g ORDER BY g",
    );
    // disc median: a→2, b→10 ; mode: a→1 (all unique, smallest), b→10
    assert_eq!(r[0][1], Value::Int(2));
    assert_eq!(r[0][2], Value::Int(1));
    assert_eq!(r[1][1], Value::Int(10));
    assert_eq!(r[1][2], Value::Int(10));
}

/// Ordered-set aggregate without WITHIN GROUP is a hard error (PG
/// parity), not a panic or a wrong number.
#[test]
fn ordered_set_requires_within_group() {
    let mut db = Database::open_in_memory();
    seed_nums(&mut db);
    let err = db
        .execute("SELECT percentile_cont(0.5) FROM t")
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("WITHIN GROUP"),
        "expected a WITHIN GROUP requirement error, got {err:?}"
    );
}

/// Statistical aggregates: PG semantics — `variance`==`var_samp`,
/// `stddev`==`stddev_samp`; samp over n=1 → NULL, pop over n=1 → 0.
#[test]
fn statistical_aggregates() {
    let mut db = Database::open_in_memory();
    seed_nums(&mut db);
    let r = rows_of(
        &mut db,
        "SELECT g, var_pop(x), var_samp(x), variance(x) \
         FROM t GROUP BY g ORDER BY g",
    );
    // a = {1,2,3,4}: var_pop = 1.25, var_samp = variance = 5/3 (NUMERIC, PG type).
    assert!((num_f64(&r[0][1]) - 1.25).abs() < 1e-9);
    assert_eq!(r[0][2], r[0][3]); // variance is var_samp
    assert!((num_f64(&r[0][2]) - 5.0 / 3.0).abs() < 1e-9);
    // single-row group: var_samp/stddev → NULL, var_pop → 0.
    db.execute("CREATE TABLE one (x INT)").unwrap();
    db.execute("INSERT INTO one VALUES (7)").unwrap();
    let r = rows_of(
        &mut db,
        "SELECT var_pop(x), var_samp(x), stddev_samp(x) FROM one",
    );
    assert!((num_f64(&r[0][0]) - 0.0).abs() < 1e-9);
    assert_eq!(r[0][1], Value::Null);
    assert_eq!(r[0][2], Value::Null);
}

/// `stddev_pop` is the square root of `var_pop`.
#[test]
fn stddev_is_sqrt_of_variance() {
    let mut db = Database::open_in_memory();
    seed_nums(&mut db);
    let r = rows_of(&mut db, "SELECT stddev_pop(x) FROM t WHERE g = 'a'");
    // sqrt(1.25) = 1.1180339887… (NUMERIC over an int column, PG type).
    assert!((num_f64(&r[0][0]) - 1.118_033_988_749_895).abs() < 1e-9);
}

/// The `ALL` quantifier (the dual of `DISTINCT`, and the default) must
/// parse — ORMs emit `COUNT(ALL x)` / `SUM(ALL x)`.
#[test]
fn all_quantifier_parses() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (x INT)").unwrap();
    db.execute("INSERT INTO t VALUES (1),(2),(2),(3)").unwrap();
    let r = rows_of(&mut db, "SELECT COUNT(ALL x), SUM(ALL x) FROM t");
    assert_eq!(r[0][0], Value::BigInt(4)); // ALL keeps duplicates
    assert_eq!(r[0][1], Value::BigInt(8)); // 1+2+2+3
    // DISTINCT still dedupes.
    let r = rows_of(&mut db, "SELECT COUNT(DISTINCT x) FROM t");
    assert_eq!(r[0][0], Value::BigInt(3));
}

/// Bitwise aggregates over integer columns.
#[test]
fn bitwise_aggregates() {
    let mut db = Database::open_in_memory();
    seed_nums(&mut db);
    // b = {10, 10, 20}: AND=0, OR=30, XOR=20.
    let r = rows_of(
        &mut db,
        "SELECT bit_and(x), bit_or(x), bit_xor(x) FROM t WHERE g = 'b'",
    );
    assert_eq!(r[0][0], Value::Int(0));
    assert_eq!(r[0][1], Value::Int(30));
    assert_eq!(r[0][2], Value::Int(20));
}

/// Hypothetical-set aggregates: the rank the direct value would have if
/// inserted into the group. Values verified against PostgreSQL 18 on
/// {1,3,5,5,8} with the hypothetical 5.
#[test]
fn hypothetical_set_aggregates() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE t (x INT)").unwrap();
    db.execute("INSERT INTO t VALUES (1),(3),(5),(5),(8)")
        .unwrap();
    let r = rows_of(
        &mut db,
        "SELECT rank(5) WITHIN GROUP (ORDER BY x), \
                dense_rank(5) WITHIN GROUP (ORDER BY x), \
                percent_rank(5) WITHIN GROUP (ORDER BY x), \
                cume_dist(5) WITHIN GROUP (ORDER BY x) FROM t",
    );
    assert_eq!(r[0][0], Value::BigInt(3)); // rank
    assert_eq!(r[0][1], Value::BigInt(3)); // dense_rank
    assert_eq!(r[0][2], Value::Float(0.4)); // percent_rank = (3-1)/5
    match r[0][3] {
        Value::Float(v) => assert!((v - 5.0 / 6.0).abs() < 1e-9), // cume_dist
        ref other => panic!("expected float, got {other:?}"),
    }
    // DESC flips which side is "before": rank(5) desc = 2.
    let r = rows_of(
        &mut db,
        "SELECT rank(5) WITHIN GROUP (ORDER BY x DESC) FROM t",
    );
    assert_eq!(r[0][0], Value::BigInt(2));
    // Below / above the whole group.
    let r = rows_of(
        &mut db,
        "SELECT rank(0) WITHIN GROUP (ORDER BY x), rank(9) WITHIN GROUP (ORDER BY x) FROM t",
    );
    assert_eq!(r[0][0], Value::BigInt(1));
    assert_eq!(r[0][1], Value::BigInt(6));
}

/// Two-argument regression family `f(Y, X)`, verified on a perfect
/// line y = 2x + 1.
#[test]
fn regression_aggregates() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE p (x INT, y INT)").unwrap();
    db.execute("INSERT INTO p VALUES (1,3),(2,5),(3,7),(4,9)")
        .unwrap();
    let r = rows_of(
        &mut db,
        "SELECT regr_slope(y,x), regr_intercept(y,x), corr(y,x), regr_r2(y,x), \
                regr_count(y,x), regr_avgx(y,x), regr_avgy(y,x), \
                covar_pop(y,x), covar_samp(y,x) FROM p",
    );
    assert_eq!(r[0][0], Value::Float(2.0)); // slope
    assert_eq!(r[0][1], Value::Float(1.0)); // intercept
    assert_eq!(r[0][2], Value::Float(1.0)); // corr (perfect)
    assert_eq!(r[0][3], Value::Float(1.0)); // r2
    assert_eq!(r[0][4], Value::BigInt(4)); // count
    assert_eq!(r[0][5], Value::Float(2.5)); // avgx
    assert_eq!(r[0][6], Value::Float(6.0)); // avgy
    assert_eq!(r[0][7], Value::Float(2.5)); // covar_pop
    match r[0][8] {
        Value::Float(v) => assert!((v - 10.0 / 3.0).abs() < 1e-9), // covar_samp
        ref other => panic!("expected float, got {other:?}"),
    }
    // Only pairs where BOTH are non-NULL count.
    db.execute("INSERT INTO p VALUES (5, NULL), (NULL, 11)")
        .unwrap();
    let r = rows_of(&mut db, "SELECT regr_count(y, x) FROM p");
    assert_eq!(r[0][0], Value::BigInt(4)); // the two NULL-bearing rows excluded
}

/// JSON aggregates: array (json_agg/jsonb_agg) and object
/// (json_object_agg). Empty set → SQL NULL.
#[test]
fn json_aggregates() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE p (x INT, y INT)").unwrap();
    db.execute("INSERT INTO p VALUES (1,3),(2,5),(3,7)")
        .unwrap();
    let r = rows_of(&mut db, "SELECT json_agg(x) FROM p");
    assert_eq!(r[0][0], Value::json("[1, 2, 3]"));
    let r = rows_of(&mut db, "SELECT json_object_agg(x, y) FROM p");
    assert_eq!(
        r[0][0],
        Value::Json("{\"1\": 3, \"2\": 5, \"3\": 7}".into())
    );
    // empty → NULL (PG)
    let r = rows_of(&mut db, "SELECT json_agg(x) FROM p WHERE x > 99");
    assert_eq!(r[0][0], Value::Null);
}

/// The whole new family composes with FILTER (the decoration applies
/// before the ordered-set / statistical accumulation).
#[test]
fn new_aggregates_compose_with_filter() {
    let mut db = Database::open_in_memory();
    seed_nums(&mut db);
    // median of a's values > 1 → {2,3,4} → 3.0. Standard clause order is
    // WITHIN GROUP then FILTER.
    let r = rows_of(
        &mut db,
        "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY x) FILTER (WHERE x > 1) \
         FROM t WHERE g = 'a'",
    );
    assert_eq!(r[0][0], Value::Float(3.0));
}
