//! v7.38 (read01 P3.10) — `pg_stat_activity` canonical view with PG's
//! column names, so monitoring tools that query the standard name + shape
//! work. SPG's native `spg_stat_activity` keeps its own column names.

use spg_engine::{Engine, QueryResult};

#[test]
fn pg_stat_activity_exposes_pg_columns() {
    let mut e = Engine::new();
    let r = e.execute("SELECT * FROM pg_stat_activity").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!("expected rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    // PG 18's pg_stat_activity column set, in order.
    assert_eq!(
        names,
        vec![
            "datid",
            "datname",
            "pid",
            "leader_pid",
            "usesysid",
            "usename",
            "application_name",
            "client_addr",
            "client_hostname",
            "client_port",
            "backend_start",
            "xact_start",
            "query_start",
            "state_change",
            "wait_event_type",
            "wait_event",
            "state",
            "backend_xid",
            "backend_xmin",
            "query_id",
            "query",
            "backend_type",
        ]
    );
}
