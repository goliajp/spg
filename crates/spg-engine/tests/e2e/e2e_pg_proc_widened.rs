//! v7.37.24 (24.6) — widened pg_proc shape (20 PG-canonical
//! columns). ORM compilers and pgAdmin's function browser query
//! prolang / provolatile / proretset / proisstrict to dispatch
//! encoders or fold constant expressions; the new shape exposes
//! the same columns PG does.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.into_iter().map(|r| r.values).collect()
}

#[test]
fn pg_proc_emits_pg_canonical_columns() {
    let mut e = Engine::new();
    let r = e.execute("SELECT * FROM pg_catalog.pg_proc").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!("expected Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "oid",
        "proname",
        "pronamespace",
        "proowner",
        "prolang",
        "procost",
        "prorows",
        "provariadic",
        "prokind",
        "prosecdef",
        "proleakproof",
        "proisstrict",
        "proretset",
        "provolatile",
        "proparallel",
        "pronargs",
        "pronargdefaults",
        "prorettype",
        "proargtypes",
        "prosrc",
    ] {
        assert!(
            names.contains(&must),
            "pg_proc missing column {must}, got {names:?}"
        );
    }
}

#[test]
fn pg_proc_provolatile_v_for_now_random_current_timestamp() {
    let mut e = Engine::new();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_proc");
    // Position: 1=proname, 13=provolatile.
    let volatile_names = ["now", "random", "current_timestamp", "gen_random_uuid"];
    for name in &volatile_names {
        let row = rs
            .iter()
            .find(|r| matches!(&r[1], Value::Text(s) if s.as_ref() == *name))
            .unwrap_or_else(|| panic!("missing pg_proc row for {name}"));
        assert!(
            matches!(&row[13], Value::Text(s) if s.as_ref() == "v"),
            "provolatile for {name} should be 'v', got {:?}",
            row[13]
        );
    }
}

#[test]
fn pg_proc_provolatile_i_for_pure_scalars() {
    let mut e = Engine::new();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_proc");
    // upper / lower / length / abs are pure → 'i' immutable.
    let immutable_names = ["upper", "lower", "length", "sqrt"];
    for name in &immutable_names {
        let row = rs
            .iter()
            .find(|r| matches!(&r[1], Value::Text(s) if s.as_ref() == *name))
            .unwrap_or_else(|| panic!("missing pg_proc row for {name}"));
        assert!(
            matches!(&row[13], Value::Text(s) if s.as_ref() == "i"),
            "provolatile for {name} should be 'i', got {:?}",
            row[13]
        );
    }
}

#[test]
fn pg_proc_prolang_is_12_for_internal_builtins() {
    let mut e = Engine::new();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_proc");
    // Position 4 = prolang. All SPG-built-in functions report
    // language OID 12 (internal C function in PG).
    for row in &rs {
        assert!(
            matches!(row[4], Value::BigInt(12)),
            "prolang for {:?} should be 12",
            row[1]
        );
    }
}

#[test]
fn pg_proc_prosrc_carries_function_name() {
    let mut e = Engine::new();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_proc");
    // Position 1 = proname, 19 = prosrc. SPG synthesises prosrc
    // as the function name itself (the engine's dispatch key).
    for row in &rs {
        match (&row[1], &row[19]) {
            (Value::Text(a), Value::Text(b)) => assert_eq!(a, b, "prosrc == proname"),
            _ => panic!("proname / prosrc wrong type"),
        }
    }
}
