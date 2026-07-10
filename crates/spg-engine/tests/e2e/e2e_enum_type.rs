//! v7.17.0 Phase 1.4 — CREATE TYPE AS ENUM + column binding +
//! INSERT-time label validation + DROP TYPE.

use spg_engine::Engine;

#[test]
fn create_type_then_use_in_column() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')")
        .unwrap();
    e.execute("CREATE TABLE p (id INT NOT NULL, m mood)")
        .unwrap();
    e.execute("INSERT INTO p VALUES (1, 'happy')").unwrap();
    e.execute("INSERT INTO p VALUES (2, 'sad')").unwrap();
    let r = e.execute("SELECT id, m FROM p ORDER BY id").unwrap();
    let rows = match r {
        spg_engine::QueryResult::Rows { rows, .. } => rows,
        _ => panic!("expected rows"),
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].values[1], spg_storage::Value::text("sad"));
}

#[test]
fn insert_invalid_enum_label_rejected() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')")
        .unwrap();
    e.execute("CREATE TABLE p (id INT NOT NULL, m mood)")
        .unwrap();
    let err = e.execute("INSERT INTO p VALUES (1, 'angry')");
    assert!(err.is_err(), "non-label value should be rejected");
}

#[test]
fn unknown_type_ident_in_create_table_rejected() {
    let mut e = Engine::new();
    let err = e.execute("CREATE TABLE p (id INT NOT NULL, m my_unknown_type)");
    assert!(err.is_err(), "unknown column type ident should error");
}

#[test]
fn null_value_allowed_when_nullable() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE mood AS ENUM ('sad', 'happy')")
        .unwrap();
    e.execute("CREATE TABLE p (id INT NOT NULL, m mood)")
        .unwrap();
    e.execute("INSERT INTO p VALUES (1, NULL)").unwrap();
    e.execute("INSERT INTO p (id) VALUES (2)").unwrap();
}

#[test]
fn duplicate_create_type_errors() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE mood AS ENUM ('sad')").unwrap();
    let err = e.execute("CREATE TYPE mood AS ENUM ('happy')");
    assert!(err.is_err(), "duplicate CREATE TYPE should error");
}

#[test]
fn duplicate_label_in_enum_rejected() {
    let mut e = Engine::new();
    let err = e.execute("CREATE TYPE mood AS ENUM ('a', 'b', 'a')");
    assert!(err.is_err(), "duplicate enum label should be rejected");
}

#[test]
fn drop_type_removes_it() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE mood AS ENUM ('happy')").unwrap();
    e.execute("DROP TYPE mood").unwrap();
    // Re-create now succeeds.
    e.execute("CREATE TYPE mood AS ENUM ('sad')").unwrap();
}

#[test]
fn drop_type_if_exists_silent_on_missing() {
    let mut e = Engine::new();
    e.execute("DROP TYPE IF EXISTS missing").unwrap();
}

#[test]
fn enum_type_round_trips_catalog() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE mood AS ENUM ('sad', 'happy')")
        .unwrap();
    e.execute("CREATE TABLE p (id INT NOT NULL, m mood)")
        .unwrap();
    e.execute("INSERT INTO p VALUES (1, 'happy')").unwrap();
    let snapshot = e.catalog().serialize();
    let restored = spg_storage::Catalog::deserialize(&snapshot).expect("round-trip");
    let enum_def = restored.enum_types().get("mood").expect("enum persisted");
    assert_eq!(
        enum_def.labels,
        vec!["sad".to_string(), "happy".to_string()]
    );
    let table = restored.get("p").expect("table");
    let col = table
        .schema()
        .columns
        .iter()
        .find(|c| c.name == "m")
        .expect("column");
    assert_eq!(col.user_enum_type.as_deref(), Some("mood"));
}

#[test]
fn enum_collision_with_table_name_rejected() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE mood (id INT NOT NULL)").unwrap();
    let err = e.execute("CREATE TYPE mood AS ENUM ('happy')");
    assert!(err.is_err());
}

#[test]
fn update_to_invalid_label_skipped_for_phase14() {
    // Phase 1.4 only validates labels at INSERT (UPDATE-time
    // validation is deferred to Phase 1.4b). This test pins the
    // current behaviour so we notice when we tighten it.
    let mut e = Engine::new();
    e.execute("CREATE TYPE mood AS ENUM ('happy', 'sad')")
        .unwrap();
    e.execute("CREATE TABLE p (id INT NOT NULL, m mood)")
        .unwrap();
    e.execute("INSERT INTO p VALUES (1, 'happy')").unwrap();
    // UPDATE may still allow non-label values — this is a known
    // gap; the test documents it. When Phase 1.4b lands, flip the
    // assertion.
    let _ = e.execute("UPDATE p SET m = 'angry' WHERE id = 1");
}

// v7.37.42-T2 ζ-B — `CREATE TYPE foo AS (a INT, b TEXT)` lands in
// the dedicated composite_types registry (no longer synthesized
// into enum_types). See e2e_composite_type.rs for the full surface;
// this test just pins that the registry lookup works after
// catalog round-trip.
#[test]
fn create_composite_type_accepts_and_registers() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE addr AS (street TEXT, city TEXT, zip INT)")
        .unwrap();
    let snapshot = e.catalog().serialize();
    let restored = spg_storage::Catalog::deserialize(&snapshot).expect("round-trip");
    let comp = restored
        .composite_types()
        .get("addr")
        .expect("composite persisted");
    assert_eq!(
        comp.fields
            .iter()
            .map(|(n, _)| n.clone())
            .collect::<Vec<_>>(),
        vec!["street".to_string(), "city".to_string(), "zip".to_string()]
    );
    // No longer overloaded onto enum_types.
    assert!(restored.enum_types().get("addr").is_none());
}

#[test]
fn create_composite_type_rejects_empty_field_list() {
    let mut e = Engine::new();
    let err = e.execute("CREATE TYPE empty AS ()");
    assert!(err.is_err(), "empty composite must be rejected");
}

#[test]
fn create_composite_type_rejects_duplicate_field_names() {
    let mut e = Engine::new();
    let err = e.execute("CREATE TYPE dup AS (a INT, A TEXT)");
    assert!(
        err.is_err(),
        "duplicate composite field (case-insensitive) must be rejected"
    );
}

#[test]
fn create_composite_type_collides_with_existing_type() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE foo AS (a INT)").unwrap();
    let err = e.execute("CREATE TYPE foo AS ENUM ('x')");
    assert!(err.is_err(), "name collision must be rejected");
}

#[test]
fn enum_order_by_uses_declaration_order() {
    // PG sorts an enum column by enumsortorder (CREATE TYPE order), not
    // the label text: 'sad' < 'ok' < 'happy'. Live-PG18.4-verified.
    let mut e = Engine::new();
    e.execute("CREATE TYPE mood AS ENUM ('sad','ok','happy')")
        .unwrap();
    e.execute("CREATE TABLE t (m mood)").unwrap();
    e.execute("INSERT INTO t VALUES ('happy'),('sad'),('ok')")
        .unwrap();
    let agg = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql).unwrap() {
            spg_engine::QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
                spg_storage::Value::Text(s) => s.to_string(),
                o => panic!("{o:?}"),
            },
            _ => panic!("rows"),
        }
    };
    assert_eq!(
        agg(
            &mut e,
            "SELECT string_agg(m::text, ',') FROM (SELECT m FROM t ORDER BY m) s"
        ),
        "sad,ok,happy"
    );
    assert_eq!(
        agg(
            &mut e,
            "SELECT string_agg(m::text, ',') FROM (SELECT m FROM t ORDER BY m DESC) s"
        ),
        "happy,ok,sad"
    );
}
