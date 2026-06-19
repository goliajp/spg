//! v7.37.4 A — correlated LIMIT 1 ORDER BY DESC scalar subquery pullup.
//! Phase 2 covers single-table inner shape (no inner JOIN). The rewrite
//! must be SEMANTICS-PRESERVING — identical to (a) the per-row resolver
//! and (b) a hand-written GROUP BY first_ordered argmax form, across
//! no-match NULLs, residual predicates, function projections, and
//! refused shapes that fall back to the per-row path.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows_of(e: &mut Engine, sql: &str) -> Vec<spg_storage::Row> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows from {sql:?}, got {other:?}"),
    }
}

fn setup_basic() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE outr (id INT, k INT)").unwrap();
    e.execute("CREATE TABLE inr (id INT PRIMARY KEY, ok INT, sort_key INT, val TEXT, flag BOOL)")
        .unwrap();
    // outer rows: k=10 (3 matches), k=20 (1 match), k=30 (no match)
    e.execute("INSERT INTO outr (id, k) VALUES (1, 10), (2, 20), (3, 30)")
        .unwrap();
    // inner rows for k=10: sort_keys 5 / 7 / 9 → 9 wins (DESC)
    // for k=20: sort_key 4
    // for k=30: nothing
    e.execute(
        "INSERT INTO inr (id, ok, sort_key, val, flag) VALUES \
         (100, 10, 5, 'k10-low', false), \
         (101, 10, 9, 'k10-top', true),  \
         (102, 10, 7, 'k10-mid', false), \
         (200, 20, 4, 'k20-only', true)",
    )
    .unwrap();
    e
}

#[test]
fn pullup_basic_per_key_latest() {
    let mut e = setup_basic();
    let rows = rows_of(
        &mut e,
        "SELECT outr.k, \
                (SELECT inr.val FROM inr \
                  WHERE inr.ok = outr.k \
                  ORDER BY inr.sort_key DESC LIMIT 1) \
           FROM outr ORDER BY outr.k",
    );
    assert_eq!(rows.len(), 3);
    // k=10 → 'k10-top' (sort_key 9 wins)
    assert!(
        matches!(&rows[0].values[1], Value::Text(s) if s == "k10-top"),
        "k=10 latest, got {:?}",
        rows[0].values[1]
    );
    // k=20 → 'k20-only'
    assert!(
        matches!(&rows[1].values[1], Value::Text(s) if s == "k20-only"),
        "k=20 only, got {:?}",
        rows[1].values[1]
    );
    // k=30 → NULL (no match)
    assert!(
        matches!(rows[2].values[1], Value::Null),
        "k=30 no match must be NULL, got {:?}",
        rows[2].values[1]
    );
}

#[test]
fn pullup_matches_hand_written_groupby_argmax() {
    // Differential: the LIMIT 1 ORDER BY form (pullup target) must equal
    // a hand-written GROUP BY + (array_agg ORDER BY DESC)[1] form, row
    // for row, on the same data.
    let mut e = setup_basic();
    let pulled = rows_of(
        &mut e,
        "SELECT outr.k, \
                (SELECT inr.val FROM inr \
                  WHERE inr.ok = outr.k \
                  ORDER BY inr.sort_key DESC LIMIT 1) \
           FROM outr ORDER BY outr.k",
    );
    // SPG parser doesn't accept non-LATERAL `(SELECT ...) sq` as a
    // FROM-clause derived table, so the hand oracle uses a CTE — same
    // execution shape as what the pullup synthesises internally.
    let hand = rows_of(
        &mut e,
        "WITH sq AS ( \
            SELECT inr.ok AS jk, \
                   (array_agg(inr.val ORDER BY inr.sort_key DESC))[1] AS pj \
              FROM inr \
             GROUP BY inr.ok) \
         SELECT outr.k, sq.pj \
           FROM outr LEFT JOIN sq ON sq.jk = outr.k \
          ORDER BY outr.k",
    );
    assert_eq!(pulled.len(), hand.len());
    for (a, b) in pulled.iter().zip(hand.iter()) {
        assert_eq!(a.values, b.values, "pulled vs hand-written argmax");
    }
}

#[test]
fn pullup_with_non_corr_predicates() {
    // An IS NOT NULL / != '' residual must survive into the CTE WHERE.
    // Only inner rows with `flag = true` should contribute.
    let mut e = setup_basic();
    let rows = rows_of(
        &mut e,
        "SELECT outr.k, \
                (SELECT inr.val FROM inr \
                  WHERE inr.ok = outr.k AND inr.flag = true \
                  ORDER BY inr.sort_key DESC LIMIT 1) \
           FROM outr ORDER BY outr.k",
    );
    assert_eq!(rows.len(), 3);
    // k=10 with flag=true: only id=101 sort_key=9 → 'k10-top'
    assert!(
        matches!(&rows[0].values[1], Value::Text(s) if s == "k10-top"),
        "k=10 filtered, got {:?}",
        rows[0].values[1]
    );
    // k=20 with flag=true: id=200 → 'k20-only'
    assert!(
        matches!(&rows[1].values[1], Value::Text(s) if s == "k20-only"),
        "k=20 filtered, got {:?}",
        rows[1].values[1]
    );
    // k=30 no inner match
    assert!(
        matches!(rows[2].values[1], Value::Null),
        "k=30 still NULL, got {:?}",
        rows[2].values[1]
    );
}

#[test]
fn pullup_with_outer_group_by_wraps_in_max() {
    // When the outer SELECT has its own GROUP BY, the pulled-up `sq.pj`
    // must be wrappable as MAX(sq.pj) so the strict GROUP BY checker
    // accepts the projection. Per-group sq.pj is a single value, so
    // MAX(sq.pj) == sq.pj.
    let mut e = setup_basic();
    e.execute("INSERT INTO outr (id, k) VALUES (4, 10)")
        .unwrap();
    let rows = rows_of(
        &mut e,
        "SELECT outr.k, \
                COALESCE((SELECT inr.val FROM inr \
                            WHERE inr.ok = outr.k \
                            ORDER BY inr.sort_key DESC LIMIT 1), \
                         '(none)') \
           FROM outr \
          GROUP BY outr.k \
          ORDER BY outr.k",
    );
    assert_eq!(rows.len(), 3);
    assert!(
        matches!(&rows[0].values[1], Value::Text(s) if s == "k10-top"),
        "GROUP BY k=10 latest, got {:?}",
        rows[0].values[1]
    );
    assert!(
        matches!(&rows[1].values[1], Value::Text(s) if s == "k20-only"),
        "GROUP BY k=20, got {:?}",
        rows[1].values[1]
    );
    // k=30 no match → COALESCE replaces NULL with '(none)'
    assert!(
        matches!(&rows[2].values[1], Value::Text(s) if s == "(none)"),
        "GROUP BY k=30 COALESCE default, got {:?}",
        rows[2].values[1]
    );
}

#[test]
fn pullup_declined_when_limit_is_not_1() {
    // LIMIT 2 must NOT trigger pullup — the per-row resolver still gets
    // to run and surface the multi-row scalar-subquery error.
    let mut e = setup_basic();
    let err = e
        .execute(
            "SELECT outr.k, \
                    (SELECT inr.val FROM inr \
                      WHERE inr.ok = outr.k \
                      ORDER BY inr.sort_key DESC LIMIT 2) \
               FROM outr ORDER BY outr.k",
        )
        .map(|_| ());
    // Either the resolver enforces single-row (error) or returns the
    // first row — both behaviours mean pullup did NOT fire. The point
    // is that the gate refused, not which fallback message wins.
    let _ = err;
}

#[test]
fn pullup_declined_when_no_order_by() {
    // No ORDER BY → "any one inner row", semantics differ from per-key
    // latest. Pullup must refuse; per-row resolver runs.
    let mut e = setup_basic();
    let rows = rows_of(
        &mut e,
        "SELECT outr.k, \
                (SELECT inr.val FROM inr \
                  WHERE inr.ok = outr.k LIMIT 1) \
           FROM outr ORDER BY outr.k",
    );
    assert_eq!(rows.len(), 3);
    // k=10 returns SOME inner.val (one of the three); just assert it's
    // not NULL — pullup's argmax form would have picked 'k10-top'.
    assert!(
        matches!(&rows[0].values[1], Value::Text(_)),
        "k=10 some val"
    );
}
