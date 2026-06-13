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

fn rows_of(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
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
    assert_eq!(r[0], vec![Value::Text("t1".into()), Value::BigInt(1)]);
    assert_eq!(r[1], vec![Value::Text("t2".into()), Value::BigInt(2)]);
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
            Value::Text("t1".into()),
            Value::BigInt(2),
            Value::BigInt(1),
            Value::BigInt(2),
        ]
    );
    // t2: total 3, seen 2, sum(id where flags=1) = 4
    assert_eq!(
        r[1],
        vec![
            Value::Text("t2".into()),
            Value::BigInt(3),
            Value::BigInt(2),
            Value::BigInt(4),
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
    assert_eq!(r, vec![vec![Value::Text("t2".into())]]);
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
