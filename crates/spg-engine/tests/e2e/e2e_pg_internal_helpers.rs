//! v7.37.17 (17.6 siblings) — information_schema._pg_* internal
//! helpers (SQLAlchemy / asyncpg / JDBC introspection).

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn int(v: &spg_storage::Value<'_>) -> i32 {
    match v {
        spg_storage::Value::Int(n) => *n,
        other => panic!("expected Int, got {other:?}"),
    }
}

#[test]
fn char_max_length_typmod_math() {
    let mut e = Engine::new();
    // varchar(255): typid 1043, typmod 255 + 4 = 259.
    assert_eq!(
        int(&first(&mut e, "SELECT _pg_char_max_length(1043, 259)")),
        255
    );
    // Octet length = 4x worst-case UTF-8.
    assert_eq!(
        int(&first(&mut e, "SELECT _pg_char_octet_length(1043, 259)")),
        1020
    );
    // Unconstrained varchar (typmod -1) → NULL.
    assert!(matches!(
        first(&mut e, "SELECT _pg_char_max_length(1043, -1)"),
        spg_storage::Value::Null
    ));
    // Non-char type → NULL.
    assert!(matches!(
        first(&mut e, "SELECT _pg_char_max_length(23, 259)"),
        spg_storage::Value::Null
    ));
}

#[test]
fn numeric_precision_scale_typmod_math() {
    let mut e = Engine::new();
    // numeric(10,2): typmod = ((10 << 16) | 2) + 4 = 655366.
    assert_eq!(
        int(&first(&mut e, "SELECT _pg_numeric_precision(1700, 655366)")),
        10
    );
    assert_eq!(
        int(&first(&mut e, "SELECT _pg_numeric_scale(1700, 655366)")),
        2
    );
    // Integer types report their bit width.
    assert_eq!(
        int(&first(&mut e, "SELECT _pg_numeric_precision(23, -1)")),
        32
    );
    assert_eq!(
        int(&first(&mut e, "SELECT _pg_numeric_precision(20, -1)")),
        64
    );
    assert_eq!(int(&first(&mut e, "SELECT _pg_numeric_scale(23, -1)")), 0);
}

#[test]
fn datetime_precision() {
    let mut e = Engine::new();
    // date → 0; timestamp default → 6; explicit typmod wins.
    assert_eq!(
        int(&first(&mut e, "SELECT _pg_datetime_precision(1082, -1)")),
        0
    );
    assert_eq!(
        int(&first(&mut e, "SELECT _pg_datetime_precision(1114, -1)")),
        6
    );
    assert_eq!(
        int(&first(&mut e, "SELECT _pg_datetime_precision(1114, 3)")),
        3
    );
}

#[test]
fn record_internals_return_null() {
    let mut e = Engine::new();
    for f in &[
        "_pg_expandarray(ARRAY[1,2])",
        "_pg_index_position(1, 1)",
        "_pg_truetypid(NULL, NULL)",
        "_pg_truetypmod(NULL, NULL)",
        "_pg_interval_type(1186, -1)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
