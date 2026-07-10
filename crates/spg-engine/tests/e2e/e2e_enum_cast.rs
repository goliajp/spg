//! v7.38 (read01 P6.67) — casting to a user ENUM validates the label against
//! the enum's members: a member passes, a non-member errors (as PG does), and
//! a NULL is a valid typed null. Oracle behaviour from live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn scalar(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn enum_cast_validates_membership() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE mood AS ENUM ('sad','ok','happy')")
        .unwrap();
    assert_eq!(
        scalar(&mut e, "SELECT 'happy'::mood"),
        spg_storage::Value::text("happy")
    );
    assert_eq!(
        scalar(&mut e, "SELECT 'sad'::mood"),
        spg_storage::Value::text("sad")
    );
    // A non-member is rejected.
    assert!(e.execute("SELECT 'bad'::mood").is_err());
    // A typed NULL passes through.
    assert_eq!(
        scalar(&mut e, "SELECT NULL::mood"),
        spg_storage::Value::Null
    );
    // Equality on the validated label works.
    assert_eq!(
        scalar(&mut e, "SELECT ('ok'::mood = 'ok')"),
        spg_storage::Value::Bool(true)
    );
}
