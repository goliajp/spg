//! UUID type + `gen_random_uuid()` / `uuid_generate_v4()`.
//!
//! PG-canonical UUID:
//!   * 16-byte value, displayed as canonical lowercase hyphenated
//!     8-4-4-4-12 form: `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`.
//!   * Accepts on input: hyphenated, unhyphenated, braced, case-
//!     insensitive hex.
//!   * `gen_random_uuid()` → RFC 4122 v4 (random) — version nibble
//!     = 4, variant top bits = 10 (10xxxxxx).
//!   * `uuid_generate_v4()` is the historical uuid-ossp alias.
//!   * Comparison: byte-wise.
//!   * Cast text → uuid: malformed input is an error.
//!
//! Required for Django/Rails/Hibernate "id UUID PRIMARY KEY DEFAULT
//! gen_random_uuid()" — the modern default ORM PK shape.

use spg_engine::{Engine, QueryResult};
use spg_storage::{DataType, Value};

fn one_row(r: QueryResult) -> Vec<Value<'static>> {
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            rows.into_iter().next().unwrap().values
        }
        _ => panic!(),
    }
}

fn text_col(e: &mut Engine, sql: &str) -> String {
    let row = one_row(
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}")),
    );
    match &row[0] {
        Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

// ── DataType + parse + display ───────────────────────────────────

#[test]
fn create_table_uuid_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id UUID NOT NULL)").unwrap();
    let r = e.execute("SELECT id FROM u").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!()
    };
    assert_eq!(columns[0].ty, DataType::Uuid);
}

#[test]
fn insert_canonical_hyphenated_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id UUID NOT NULL)").unwrap();
    e.execute("INSERT INTO u VALUES ('550e8400-e29b-41d4-a716-446655440000')")
        .unwrap();
    // SELECT renders canonical lowercase hyphenated form.
    let r = e.execute("SELECT id FROM u").unwrap();
    let QueryResult::Rows { rows, columns } = r else {
        panic!()
    };
    assert_eq!(columns[0].ty, DataType::Uuid);
    match &rows[0].values[0] {
        Value::Uuid(b) => {
            assert_eq!(b[0], 0x55);
            assert_eq!(b[15], 0x00);
        }
        other => panic!("expected Value::Uuid, got {other:?}"),
    }
}

#[test]
fn parse_uppercase_normalized_to_lowercase() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id UUID NOT NULL)").unwrap();
    e.execute("INSERT INTO u VALUES ('550E8400-E29B-41D4-A716-446655440000')")
        .unwrap();
    let r = e.execute("SELECT id::text FROM u").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(
        rows[0].values[0],
        Value::text("550e8400-e29b-41d4-a716-446655440000")
    );
}

#[test]
fn parse_unhyphenated_form_accepted() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id UUID NOT NULL)").unwrap();
    e.execute("INSERT INTO u VALUES ('550e8400e29b41d4a716446655440000')")
        .unwrap();
    let r = e.execute("SELECT id::text FROM u").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(
        rows[0].values[0],
        Value::text("550e8400-e29b-41d4-a716-446655440000")
    );
}

#[test]
fn parse_braced_form_accepted() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id UUID NOT NULL)").unwrap();
    e.execute("INSERT INTO u VALUES ('{550e8400-e29b-41d4-a716-446655440000}')")
        .unwrap();
    let r = e.execute("SELECT id::text FROM u").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(
        rows[0].values[0],
        Value::text("550e8400-e29b-41d4-a716-446655440000")
    );
}

#[test]
fn malformed_uuid_input_errors() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id UUID NOT NULL)").unwrap();
    assert!(e.execute("INSERT INTO u VALUES ('not-a-uuid')").is_err());
    assert!(
        e.execute("INSERT INTO u VALUES ('550e8400-e29b-41d4-a716-44665544000')")
            .is_err()
    ); // too short
    assert!(
        e.execute("INSERT INTO u VALUES ('550e8400-e29b-41d4-a716-4466554400000')")
            .is_err()
    ); // too long
    assert!(
        e.execute("INSERT INTO u VALUES ('550e8400-e29b-41d4-a716-44665544000g')")
            .is_err()
    ); // non-hex
}

// ── gen_random_uuid() ────────────────────────────────────────────

#[test]
fn gen_random_uuid_returns_uuid_type() {
    let mut e = Engine::new();
    let r = e.execute("SELECT gen_random_uuid()").unwrap();
    let QueryResult::Rows { rows, columns } = r else {
        panic!()
    };
    assert_eq!(columns[0].ty, DataType::Uuid);
    assert!(matches!(rows[0].values[0], Value::Uuid(_)));
}

#[test]
fn gen_random_uuid_v4_version_and_variant_bits() {
    // RFC 4122: byte 6 high nibble = 4 (version),
    // byte 8 high 2 bits = 10 (variant 1).
    let mut e = Engine::new();
    for _ in 0..20 {
        let row = one_row(e.execute("SELECT gen_random_uuid()").unwrap());
        let Value::Uuid(b) = row[0] else {
            panic!("expected Uuid");
        };
        assert_eq!(b[6] & 0xf0, 0x40, "version nibble must be 4");
        assert_eq!(b[8] & 0xc0, 0x80, "variant top 2 bits must be 10");
    }
}

#[test]
fn gen_random_uuid_is_unique_across_calls() {
    let mut e = Engine::new();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..50 {
        let row = one_row(e.execute("SELECT gen_random_uuid()").unwrap());
        let Value::Uuid(b) = row[0] else { panic!() };
        assert!(seen.insert(b), "gen_random_uuid produced a collision");
    }
}

#[test]
fn uuid_generate_v4_alias_works() {
    // uuid-ossp historical alias.
    let mut e = Engine::new();
    let r = e.execute("SELECT uuid_generate_v4()").unwrap();
    let QueryResult::Rows { rows, columns } = r else {
        panic!()
    };
    assert_eq!(columns[0].ty, DataType::Uuid);
    let Value::Uuid(b) = rows[0].values[0] else {
        panic!()
    };
    assert_eq!(b[6] & 0xf0, 0x40);
    assert_eq!(b[8] & 0xc0, 0x80);
}

#[test]
fn gen_random_uuid_arity_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT gen_random_uuid('x')").is_err());
}

#[test]
fn gen_random_uuid_default_pk_pattern() {
    // The Django/Rails/Hibernate default PK pattern.
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id UUID NOT NULL DEFAULT gen_random_uuid(), name TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u (name) VALUES ('alice'), ('bob'), ('carol')")
        .unwrap();
    let r = e.execute("SELECT id FROM u").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 3);
    let mut seen = std::collections::HashSet::new();
    for row in &rows {
        let Value::Uuid(b) = row.values[0] else {
            panic!("non-Uuid value")
        };
        assert_eq!(b[6] & 0xf0, 0x40);
        assert_eq!(b[8] & 0xc0, 0x80);
        assert!(seen.insert(b), "DEFAULT collided");
    }
}

// ── Comparison + equality ────────────────────────────────────────

#[test]
fn uuid_equality_byte_wise() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id UUID NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute(
        "INSERT INTO u VALUES \
         ('550e8400-e29b-41d4-a716-446655440000', 'alice'), \
         ('00000000-0000-0000-0000-000000000000', 'bob')",
    )
    .unwrap();
    let r = e
        .execute("SELECT name FROM u WHERE id = '550e8400-e29b-41d4-a716-446655440000'")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::text("alice"));
}

#[test]
fn uuid_uppercase_input_equals_lowercase() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id UUID NOT NULL)").unwrap();
    e.execute("INSERT INTO u VALUES ('550e8400-e29b-41d4-a716-446655440000')")
        .unwrap();
    let r = e
        .execute("SELECT COUNT(*) FROM u WHERE id = '550E8400-E29B-41D4-A716-446655440000'")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    match &rows[0].values[0] {
        Value::Int(n) => assert_eq!(*n, 1),
        Value::BigInt(n) => assert_eq!(*n, 1),
        other => panic!("got {other:?}"),
    }
}

// ── Cast ─────────────────────────────────────────────────────────

#[test]
fn cast_text_to_uuid() {
    let mut e = Engine::new();
    let r = e
        .execute("SELECT '550e8400-e29b-41d4-a716-446655440000'::uuid")
        .unwrap();
    let QueryResult::Rows { rows, columns } = r else {
        panic!()
    };
    assert_eq!(columns[0].ty, DataType::Uuid);
    let Value::Uuid(b) = rows[0].values[0] else {
        panic!()
    };
    assert_eq!(b[0], 0x55);
}

#[test]
fn cast_uuid_to_text_canonical_form() {
    let mut e = Engine::new();
    assert_eq!(
        text_col(
            &mut e,
            "SELECT '550E8400-E29B-41D4-A716-446655440000'::uuid::text"
        ),
        "550e8400-e29b-41d4-a716-446655440000"
    );
}

#[test]
fn cast_malformed_text_to_uuid_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT 'not-a-uuid'::uuid").is_err());
}

// ── NULL handling ────────────────────────────────────────────────

#[test]
fn nullable_uuid_accepts_null() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id UUID, name TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (NULL, 'alice')").unwrap();
    let r = e.execute("SELECT id FROM u").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows[0].values[0], Value::Null);
}
