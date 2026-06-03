//! v7.3.0 — `Database::query_typed::<T>` + `spg_row!` macro.

use spg_embedded::{Database, FromSpgRow, FromSpgValue, Value, spg_row};

spg_row! {
    pub struct User {
        pub id: i32,
        pub name: String,
    }
}

spg_row! {
    pub struct UserOpt {
        pub id: i32,
        pub name: Option<String>,
    }
}

spg_row! {
    pub struct Embedding {
        pub id: i32,
        pub v: Vec<f32>,
    }
}

#[test]
fn typed_query_basic_user() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'alice')").unwrap();
    db.execute("INSERT INTO users VALUES (2, 'bob')").unwrap();
    let mut users: Vec<User> = db.query_typed("SELECT id, name FROM users").unwrap();
    users.sort_by_key(|u| u.id);
    assert_eq!(users.len(), 2);
    assert_eq!(users[0].id, 1);
    assert_eq!(users[0].name, "alice");
    assert_eq!(users[1].id, 2);
    assert_eq!(users[1].name, "bob");
}

#[test]
fn typed_query_with_optional_field_handles_null() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE users (id INT NOT NULL, name TEXT)").unwrap();
    db.execute("INSERT INTO users VALUES (1, 'alice')").unwrap();
    db.execute("INSERT INTO users VALUES (2, NULL)").unwrap();
    let mut users: Vec<UserOpt> = db.query_typed("SELECT id, name FROM users").unwrap();
    users.sort_by_key(|u| u.id);
    assert_eq!(users[0].name, Some("alice".to_string()));
    assert_eq!(users[1].name, None);
}

#[test]
fn typed_query_with_vector_column() {
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE emb (id INT NOT NULL, v VECTOR(4) NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO emb VALUES (1, [1.0, 2.0, 3.0, 4.0])")
        .unwrap();
    let rows: Vec<Embedding> = db.query_typed("SELECT id, v FROM emb WHERE id = 1").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, 1);
    assert_eq!(rows[0].v, vec![1.0, 2.0, 3.0, 4.0]);
}

#[test]
fn manual_from_spg_row_still_works() {
    // Hand-written impl (no macro) — for users who want
    // custom decoding logic.
    struct CustomUser {
        id: i64,
        upper_name: String,
    }
    impl FromSpgRow for CustomUser {
        fn from_spg_row(row: &[Value]) -> Result<Self, spg_embedded::EngineError> {
            let id = i64::from_spg_value(&row[0])
                .map_err(|e| spg_embedded::EngineError::Unsupported(e.into()))?;
            let name = String::from_spg_value(&row[1])
                .map_err(|e| spg_embedded::EngineError::Unsupported(e.into()))?;
            Ok(Self {
                id,
                upper_name: name.to_uppercase(),
            })
        }
    }
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE u (id BIGINT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO u VALUES (42, 'alice')").unwrap();
    let users: Vec<CustomUser> = db.query_typed("SELECT id, name FROM u").unwrap();
    assert_eq!(users[0].id, 42);
    assert_eq!(users[0].upper_name, "ALICE");
}

#[test]
fn type_mismatch_surfaces_error() {
    // Asking for `i32` against a TEXT column should error
    // cleanly, not panic.
    spg_row! {
        struct Wrong {
            id: i32,
            name: i32,  // wrong — TEXT column.
        }
    }
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE u (id INT NOT NULL, name TEXT NOT NULL)").unwrap();
    db.execute("INSERT INTO u VALUES (1, 'alice')").unwrap();
    let r: Result<Vec<Wrong>, _> = db.query_typed("SELECT id, name FROM u");
    assert!(r.is_err());
}

#[test]
fn missing_column_surfaces_error() {
    // SELECT one column but FromSpgRow expects two → error.
    let mut db = Database::open_in_memory();
    db.execute("CREATE TABLE u (id INT NOT NULL, name TEXT NOT NULL)").unwrap();
    db.execute("INSERT INTO u VALUES (1, 'alice')").unwrap();
    let r: Result<Vec<User>, _> = db.query_typed("SELECT id FROM u");
    assert!(r.is_err());
}
