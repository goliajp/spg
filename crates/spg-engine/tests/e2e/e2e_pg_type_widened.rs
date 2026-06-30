//! v7.37.24 (24.7) — widened pg_type shape (22 PG-canonical
//! columns). Encoders / decoders (sqlx, Diesel, SQLAlchemy,
//! pgAdmin's type explorer) query specific columns to pick
//! encoding strategies; the new shape exposes the same columns
//! PG does so introspection lookups don't need an SPG-specific
//! branch.

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
fn pg_type_emits_pg_canonical_columns() {
    let mut e = Engine::new();
    let r = e.execute("SELECT * FROM pg_catalog.pg_type").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!("expected Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "oid",
        "typname",
        "typnamespace",
        "typowner",
        "typlen",
        "typbyval",
        "typtype",
        "typcategory",
        "typispreferred",
        "typisdefined",
        "typdelim",
        "typrelid",
        "typelem",
        "typarray",
        "typalign",
        "typstorage",
        "typnotnull",
        "typbasetype",
        "typtypmod",
        "typndims",
        "typcollation",
    ] {
        assert!(
            names.contains(&must),
            "pg_type missing column {must}, got {names:?}"
        );
    }
}

#[test]
fn pg_type_typbyval_matches_pg_pass_by_value_set() {
    let mut e = Engine::new();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_type");
    // Columns: 0=oid, 1=typname, 4=typlen, 5=typbyval.
    let row_for = |oid: i64| {
        rs.iter()
            .find(|r| matches!(r[0], Value::BigInt(o) if o == oid))
            .unwrap_or_else(|| panic!("oid {oid} not in pg_type"))
    };
    // PG pass-by-value: int4 (oid 23), int8 (20), bool (16), oid (26).
    for oid in [23i64, 20, 16, 26] {
        let r = row_for(oid);
        assert!(
            matches!(r[5], Value::Bool(true)),
            "typbyval for oid {oid} should be true"
        );
    }
    // Var-length pass-by-reference: text (25), bytea (17), numeric (1700).
    for oid in [25i64, 17, 1700] {
        let r = row_for(oid);
        assert!(
            matches!(r[5], Value::Bool(false)),
            "typbyval for oid {oid} should be false"
        );
    }
}

#[test]
fn pg_type_array_companion_typelem_points_at_scalar() {
    let mut e = Engine::new();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_type");
    // _int4 (1007) — typelem should be int4 (23).
    let row = rs
        .iter()
        .find(|r| matches!(r[0], Value::BigInt(1007)))
        .expect("_int4 row");
    // Position 12 = typsubscript, 13 = typelem.
    assert!(matches!(row[13], Value::BigInt(23)), "typelem for _int4");
    // _text (1009) — typelem should be text (25).
    let row = rs
        .iter()
        .find(|r| matches!(r[0], Value::BigInt(1009)))
        .expect("_text row");
    assert!(matches!(row[13], Value::BigInt(25)), "typelem for _text");
}

#[test]
fn pg_type_typdelim_is_comma_for_all_builtins() {
    let mut e = Engine::new();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_type");
    // Position 10 = typdelim.
    for r in &rs {
        assert!(
            matches!(&r[10], Value::Text(s) if s.as_ref() == ","),
            "all built-in typdelim should be ','"
        );
    }
}
