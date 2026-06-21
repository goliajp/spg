//! INNER join with a WHERE predicate on the NON-driver (peer) table must
//! stay correct after v7.33 (mailrs 7.33.0) deferred those peers to the
//! index-nested-loop path: seek the driver, look up only matched peer
//! rows, then apply the peer predicate as a residual filter — instead of
//! eagerly scanning + filtering the whole peer table. These pin that the
//! residual filter drops exactly the right (left,right) pairs.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn setup() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE drv (id INT, sk INT)").unwrap();
    e.execute("CREATE INDEX drv_sk ON drv(sk)").unwrap();
    e.execute("CREATE TABLE peer (pid INT PRIMARY KEY, cat TEXT)")
        .unwrap();
    // sk=5 → ids 1,2,3 ; id 4 has sk=9 (not seeked by sk=5).
    e.execute("INSERT INTO drv (id, sk) VALUES (1,5),(2,5),(3,5),(4,9)")
        .unwrap();
    // pid 2 has an empty cat → must be filtered by `peer.cat != ''`.
    e.execute("INSERT INTO peer (pid, cat) VALUES (1,'a'),(2,''),(3,'c'),(4,'d')")
        .unwrap();
    e
}

fn rows(e: &mut Engine, sql: &str) -> Vec<spg_storage::Row<'static>> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows from {sql:?}, got {other:?}"),
    }
}

#[test]
fn inner_join_peer_predicate_filters_matched_pairs() {
    let mut e = setup();
    // Seek drv on sk=5 (ids 1,2,3), INL to peer by pid, drop peer.cat=''.
    // Expect id 1 (a) and id 3 (c); id 2 filtered, id 4 not seeked.
    let r = rows(
        &mut e,
        "SELECT drv.id, peer.cat FROM drv JOIN peer ON peer.pid = drv.id \
         WHERE drv.sk = 5 AND peer.cat != '' ORDER BY drv.id",
    );
    let got: Vec<(i32, String)> = r
        .iter()
        .map(|row| {
            let id = match row.values[0] {
                Value::Int(n) => n,
                ref o => panic!("{o:?}"),
            };
            let cat = match &row.values[1] {
                Value::Text(s) => s.to_string(),
                o => panic!("{o:?}"),
            };
            (id, cat)
        })
        .collect();
    assert_eq!(got, vec![(1, "a".into()), (3, "c".into())]);
}

#[test]
fn peer_predicate_equals_no_predicate_minus_filtered() {
    // Differential: the peer-predicate result must equal the unfiltered
    // join minus the rows the predicate removes.
    let mut e = setup();
    let with_pred = rows(
        &mut e,
        "SELECT drv.id FROM drv JOIN peer ON peer.pid = drv.id \
         WHERE drv.sk = 5 AND peer.cat != '' ORDER BY drv.id",
    );
    let no_pred = rows(
        &mut e,
        "SELECT drv.id FROM drv JOIN peer ON peer.pid = drv.id \
         WHERE drv.sk = 5 ORDER BY drv.id",
    );
    assert_eq!(no_pred.len(), 3, "ids 1,2,3 seeked");
    assert_eq!(with_pred.len(), 2, "id 2 (empty cat) dropped");
}

#[test]
fn peer_predicate_in_aggregate_over_thread_list() {
    // The mailrs shape: GROUP BY a seek key over an IN-list, with a peer
    // predicate. Each group aggregates only the peer rows passing the
    // predicate.
    let mut e = Engine::new();
    e.execute("CREATE TABLE m (id INT, thread INT)").unwrap();
    e.execute("CREATE INDEX m_thread ON m(thread)").unwrap();
    e.execute("CREATE TABLE ea (mid INT PRIMARY KEY, cat TEXT)")
        .unwrap();
    e.execute("INSERT INTO m (id, thread) VALUES (1,10),(2,10),(3,20),(4,20)")
        .unwrap();
    // mid 2 and 4 have empty cat → excluded by `ea.cat != ''`.
    e.execute("INSERT INTO ea (mid, cat) VALUES (1,'x'),(2,''),(3,'y'),(4,'')")
        .unwrap();
    let r = rows(
        &mut e,
        "SELECT m.thread, COUNT(*) FROM m JOIN ea ON ea.mid = m.id \
         WHERE m.thread IN (10,20) AND ea.cat != '' GROUP BY m.thread ORDER BY m.thread",
    );
    assert_eq!(r.len(), 2);
    // thread 10: only mid 1 passes → COUNT 1 ; thread 20: only mid 3 → 1.
    assert!(
        matches!(r[0].values[1], Value::BigInt(1)),
        "thread10 {:?}",
        r[0].values[1]
    );
    assert!(
        matches!(r[1].values[1], Value::BigInt(1)),
        "thread20 {:?}",
        r[1].values[1]
    );
}
