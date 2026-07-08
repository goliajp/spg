//! v7.38 (read01, T26) — current_setting on an unset namespaced custom GUC
//! errors ("unrecognized configuration parameter") rather than returning empty,
//! matching PG; the missing_ok=true form still yields NULL, and the
//! set_config/SET → current_setting round-trip is unaffected. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().next().unwrap().values.into_iter().next().unwrap(),
        _ => panic!("rows"),
    }
}

#[test]
fn current_setting_unset_custom_guc_errors() {
    let mut e = Engine::new();
    // Unset namespaced GUC: hard error without the missing_ok flag.
    assert!(e.execute("SELECT current_setting('myapp.none')").is_err());
    // missing_ok = true → NULL.
    assert!(matches!(one(&mut e, "SELECT current_setting('myapp.none', true)"), spg_storage::Value::Null));
    // Round-trip via set_config, then a normal read succeeds.
    e.execute("SELECT set_config('myapp.x', 'v', false)").unwrap();
    assert!(matches!(one(&mut e, "SELECT current_setting('myapp.x')"), spg_storage::Value::Text(ref s) if s == "v"));
    // Round-trip via SET.
    e.execute("SET myapp.y = 'w'").unwrap();
    assert!(matches!(one(&mut e, "SELECT current_setting('myapp.y')"), spg_storage::Value::Text(ref s) if s == "w"));
    // A built-in GUC still resolves.
    assert!(matches!(one(&mut e, "SELECT current_setting('search_path')"), spg_storage::Value::Text(_)));
}
