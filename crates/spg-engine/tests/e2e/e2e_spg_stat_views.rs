//! v6.5.0 — `spg_stat_replication` + `spg_stat_segment` virtual
//! tables.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows_of(res: QueryResult) -> Vec<Vec<Value<'static>>> {
    match res {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

fn columns_of(res: QueryResult) -> Vec<String> {
    match res {
        QueryResult::Rows { columns, .. } => columns.into_iter().map(|c| c.name).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn replication_lists_subscriptions() {
    let mut eng = Engine::new();
    // Create a publication so the subscription has something to
    // point at, then a subscription itself.
    eng.execute("CREATE PUBLICATION p FOR ALL TABLES").unwrap();
    eng.execute("CREATE SUBSCRIPTION s CONNECTION 'host=localhost port=5432' PUBLICATION p")
        .unwrap();

    let res = eng.execute("SELECT * FROM spg_stat_replication").unwrap();
    let cols = columns_of(eng.execute("SELECT * FROM spg_stat_replication").unwrap());
    assert_eq!(
        cols,
        vec![
            "name".to_string(),
            "conn_str".to_string(),
            "publications".to_string(),
            "last_received_pos".to_string(),
            "enabled".to_string(),
        ]
    );
    let got = rows_of(res);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0][0], Value::text("s"));
    assert_eq!(got[0][1], Value::text("host=localhost port=5432"));
    assert_eq!(got[0][2], Value::text("p"));
    assert_eq!(got[0][3], Value::BigInt(0));
    assert_eq!(got[0][4], Value::Bool(true));
}

#[test]
fn segment_lists_cold_inventory_when_empty() {
    let mut eng = Engine::new();
    let res = eng.execute("SELECT * FROM spg_stat_segment").unwrap();
    let cols = columns_of(eng.execute("SELECT * FROM spg_stat_segment").unwrap());
    // v6.7.0 — spg_stat_segment gained a `table_name` column.
    assert_eq!(
        cols,
        vec![
            "segment_id".to_string(),
            "table_name".to_string(),
            "num_rows".to_string(),
            "num_pages".to_string(),
            "total_bytes".to_string(),
        ]
    );
    let got = rows_of(res);
    assert!(got.is_empty(), "fresh engine has no cold segments");
}

#[test]
fn replication_empty_when_no_subscriptions() {
    let mut eng = Engine::new();
    let res = eng.execute("SELECT * FROM spg_stat_replication").unwrap();
    let got = rows_of(res);
    assert!(got.is_empty());
}
