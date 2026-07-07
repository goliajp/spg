//! v7.38 (read01 P6.40) — casting a value to a user DOMAIN (`x::domain`)
//! enforces the domain's NOT NULL + CHECK constraints, matching PG. (Domain
//! constraints on a table column were already enforced at INSERT.)

use spg_engine::{Engine, QueryResult};

fn scalar(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn domain_cast_enforces_check() {
    let mut e = Engine::new();
    e.execute("CREATE DOMAIN posint AS int CHECK (VALUE > 0)").unwrap();
    assert_eq!(scalar(&mut e, "SELECT 5::posint"), spg_storage::Value::Int(5));
    // Constraint violation is an error.
    assert!(e.execute("SELECT (-3)::posint").is_err());
    assert!(e.execute("SELECT 0::posint").is_err());
    // NULL passes a nullable domain's CHECK (PG semantics).
    assert_eq!(scalar(&mut e, "SELECT NULL::posint"), spg_storage::Value::Null);
}

#[test]
fn domain_cast_enforces_not_null() {
    let mut e = Engine::new();
    e.execute("CREATE DOMAIN nn AS text NOT NULL").unwrap();
    assert_eq!(scalar(&mut e, "SELECT 'hi'::nn"), spg_storage::Value::text("hi"));
    assert!(e.execute("SELECT NULL::nn").is_err());
}
