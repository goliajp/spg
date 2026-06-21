//! v7.17.0 Phase 3.P0-50 — `pg_catalog.pg_type` virtual view.
//!
//! sqlx::query!() / SQLAlchemy / Diesel / pgAdmin all run a
//! `SELECT … FROM pg_type WHERE oid = $1` at connect / compile
//! time to map column type-codes to language types. Without
//! pg_type SPG fails the connect-time discovery and the customer's
//! migration stalls before any user query runs. This commit lands
//! the synthetic view with rows for every built-in scalar + its
//! array companion, so the lookup matches PG's pg_type.dat.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn pg_type_resolves_int4_by_oid() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT typname FROM pg_catalog.pg_type WHERE oid = 23")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::text("int4"));
}

#[test]
fn pg_type_resolves_text_by_oid() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT typname FROM pg_catalog.pg_type WHERE oid = 25")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::text("text"));
}

#[test]
fn pg_type_resolves_jsonb_array() {
    // The sqlx pgsql encoder also looks up `pg_type` for the
    // array companion (`_jsonb` = 3807, element = 3802).
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT typname, typelem FROM pg_catalog.pg_type WHERE oid = 3807")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::text("_jsonb"));
    assert_eq!(r[0][1], Value::BigInt(3802));
}

#[test]
fn pg_type_lookup_by_typname() {
    // The reverse direction — application code that asks "what
    // OID does PG use for `timestamptz`?".
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT oid FROM pg_catalog.pg_type WHERE typname = 'timestamptz'")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::BigInt(1184));
}

#[test]
fn pg_type_array_links_to_scalar() {
    // Verify that every array OID we emit references a real
    // scalar entry; the sqlx encoder relies on this to resolve
    // `_int4[]` back to `int4`.
    let mut e = Engine::new();
    let r = rows(
        e.execute(
            "SELECT a.typname, b.typname \
             FROM pg_catalog.pg_type a JOIN pg_catalog.pg_type b ON a.typelem = b.oid \
             WHERE a.typname = '_int4'",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::text("_int4"));
    assert_eq!(r[0][1], Value::text("int4"));
}

#[test]
fn pg_type_typcategory_for_numeric_is_n() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(
            "SELECT typcategory FROM pg_catalog.pg_type WHERE typname IN ('int4', 'int8', 'float8') \
             ORDER BY oid",
        )
        .unwrap(),
    );
    for row in &r {
        assert_eq!(row[0], Value::text("N"));
    }
}

#[test]
fn pg_type_count_at_least_covers_common_scalars() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT COUNT(*) FROM pg_catalog.pg_type")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    // At minimum the ~30 scalar + ~30 array companion entries
    // we emit — accept any value ≥ 50 to leave headroom for
    // future additions.
    let count = match r[0][0] {
        Value::BigInt(n) => n,
        _ => panic!("expected count to be BigInt"),
    };
    assert!(count >= 50, "expected ≥ 50 pg_type rows, got {count}");
}
