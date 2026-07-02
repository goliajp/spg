//! v7.37.17 (17.6 siblings) — GIN/BRIN index maintenance probes.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn maintenance_counters_return_zero() {
    let mut e = Engine::new();
    for f in &[
        "gin_clean_pending_list('idx1')",
        "brin_summarize_new_values('idx1')",
    ] {
        let sql = format!("SELECT {f}");
        match first(&mut e, &sql) {
            spg_storage::Value::BigInt(0) => {}
            other => panic!("SELECT {f}: got {other:?}"),
        }
    }
}

#[test]
fn range_ops_return_null() {
    let mut e = Engine::new();
    for f in &[
        "brin_summarize_range('idx1', 1)",
        "brin_desummarize_range('idx1', 1)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}

#[test]
fn amvalidate_returns_true() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT amvalidate(403)") {
        spg_storage::Value::Bool(true) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn gin_support_probes_return_zero() {
    let mut e = Engine::new();
    for f in &["gin_cmp_tslexeme('a', 'b')", "gin_compare_jsonb('a', 'b')"] {
        let sql = format!("SELECT {f}");
        match first(&mut e, &sql) {
            spg_storage::Value::Int(0) => {}
            other => panic!("SELECT {f}: got {other:?}"),
        }
    }
}
