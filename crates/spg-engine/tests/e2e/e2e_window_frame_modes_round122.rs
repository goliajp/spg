//! v7.39 (read01 round 122, Track A — nodeWindowAgg.c 补读) — window frame
//! modes (GROUPS, RANGE-offset incl. date/interval, DESC, empty frames),
//! locked byte-identical against PG 18.4.
//!
//! Read-driven scan of `src/backend/executor/nodeWindowAgg.c`: no SPG
//! divergence. Complements round 109's EXCLUDE GROUP/TIES pins by locking the
//! GROUPS frame mode, type-aware RANGE offsets (numeric and date±interval),
//! DESC-order offset direction, and empty-frame aggregate semantics
//! (sum → NULL, count → 0).

use spg_engine::{Engine, QueryResult};

fn agg(e: &mut Engine, inner: &str) -> String {
    let sql = format!("SELECT string_agg(coalesce(s::text,'NULL'), '/') FROM ({inner}) q");
    match e.execute(&sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Null => "NULL".to_string(),
            v => spg_engine::eval::value_to_text(v),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

const PEERS: &str = "(VALUES(10,1),(10,2),(20,3),(20,4),(30,5)) t(g,v)";

#[test]
fn groups_frame_mode() {
    let mut e = Engine::new();
    assert_eq!(
        agg(&mut e, &format!("SELECT sum(v) OVER (ORDER BY g GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING) s FROM {PEERS} ORDER BY g")),
        "10/10/15/15/12"
    );
    assert_eq!(
        agg(&mut e, &format!("SELECT sum(v) OVER (ORDER BY g GROUPS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING) s FROM {PEERS} ORDER BY g")),
        "15/15/12/12/5"
    );
}

#[test]
fn range_offset_numeric_and_interval() {
    let mut e = Engine::new();
    assert_eq!(
        agg(&mut e, &format!("SELECT sum(v) OVER (ORDER BY g RANGE BETWEEN 5 PRECEDING AND 5 FOLLOWING) s FROM {PEERS} ORDER BY g")),
        "3/3/7/7/5"
    );
    // Type-aware RANGE offset: date column with an INTERVAL bound.
    assert_eq!(
        agg(&mut e, "SELECT sum(v) OVER (ORDER BY d RANGE BETWEEN INTERVAL '1 day' PRECEDING AND INTERVAL '1 day' FOLLOWING) s \
                     FROM (VALUES(DATE '2024-01-01',1),(DATE '2024-01-01',2),(DATE '2024-01-02',3),(DATE '2024-01-05',4)) t(d,v) ORDER BY d"),
        "6/6/6/4"
    );
    // DESC order flips the offset direction.
    assert_eq!(
        agg(&mut e, "SELECT sum(v) OVER (ORDER BY g DESC RANGE BETWEEN 5 PRECEDING AND 5 FOLLOWING) s \
                     FROM (VALUES(10,1),(15,2),(20,3)) t(g,v) ORDER BY g DESC"),
        "5/6/3"
    );
}

#[test]
fn empty_frame_aggregates() {
    let mut e = Engine::new();
    // Asymmetric future RANGE frame: the last row's frame is empty → sum NULL.
    assert_eq!(
        agg(&mut e, "SELECT sum(v) OVER (ORDER BY g RANGE BETWEEN 5 FOLLOWING AND 10 FOLLOWING) s \
                     FROM (VALUES(10,1),(15,2),(20,3),(30,4)) t(g,v) ORDER BY g"),
        "5/3/4/NULL"
    );
    // Empty ROWS frame → count 0 (not NULL).
    assert_eq!(
        agg(&mut e, "SELECT count(*) OVER (ORDER BY g ROWS BETWEEN 2 FOLLOWING AND 3 FOLLOWING) s \
                     FROM (VALUES(1,1),(2,2),(3,3),(4,4)) t(g,v) ORDER BY g"),
        "2/1/0/0"
    );
}
