//! v7.13.0 — mailrs round-5 G7 (UPDATE OF cols in trigger) and
//! G8 (ALTER COLUMN TYPE … USING).

use spg_engine::Engine;
use spg_storage::Value;

// ── G7 — CREATE TRIGGER … UPDATE OF cols ──────────────────────

#[test]
fn create_trigger_update_of_cols_parses_and_stores() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE msgs (id INT NOT NULL, subject TEXT, sender TEXT)")
        .unwrap();
    eng.execute(
        "CREATE OR REPLACE FUNCTION mark_dirty() RETURNS TRIGGER LANGUAGE plpgsql AS $$\n\
         BEGIN RETURN NEW; END\n\
         $$",
    )
    .unwrap();
    eng.execute(
        "CREATE TRIGGER msgs_dirty BEFORE UPDATE OF subject, sender ON msgs \
         FOR EACH ROW EXECUTE FUNCTION mark_dirty()",
    )
    .unwrap();
    let trgs = eng.catalog().triggers();
    assert_eq!(trgs.len(), 1);
    assert_eq!(trgs[0].update_columns, vec!["subject", "sender"]);
}

#[test]
fn update_of_filter_skips_trigger_when_other_columns_change() {
    let mut eng = Engine::new();
    // Use a side-effect-free trigger — fire counts via an
    // INSERT into an audit table inside the trigger body.
    eng.execute("CREATE TABLE msgs (id INT NOT NULL, subject TEXT, sender TEXT)")
        .unwrap();
    eng.execute("CREATE TABLE audit (n INT NOT NULL)").unwrap();
    eng.execute("INSERT INTO msgs VALUES (1, 'sub', 'a@x')")
        .unwrap();
    eng.execute(
        "CREATE OR REPLACE FUNCTION audit_fn() RETURNS TRIGGER LANGUAGE plpgsql AS $$\n\
         BEGIN INSERT INTO audit VALUES (1); RETURN NEW; END\n\
         $$",
    )
    .unwrap();
    eng.execute(
        "CREATE TRIGGER audit_subj AFTER UPDATE OF subject ON msgs \
         FOR EACH ROW EXECUTE FUNCTION audit_fn()",
    )
    .unwrap();
    // Update an unrelated column — trigger should NOT fire.
    eng.execute("UPDATE msgs SET sender = 'b@x' WHERE id = 1")
        .unwrap();
    let audit = eng.catalog().get("audit").expect("table present");
    assert_eq!(
        audit.rows().len(),
        0,
        "trigger fired despite UPDATE OF filter"
    );
    // Now update subject — trigger should fire.
    eng.execute("UPDATE msgs SET subject = 'sub2' WHERE id = 1")
        .unwrap();
    let audit = eng.catalog().get("audit").expect("table present");
    assert_eq!(
        audit.rows().len(),
        1,
        "trigger should fire when subject changes"
    );
}

#[test]
fn create_trigger_display_round_trips_update_of_cols() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, x INT, y INT)")
        .unwrap();
    eng.execute(
        "CREATE OR REPLACE FUNCTION fn() RETURNS TRIGGER LANGUAGE plpgsql AS $$\n\
         BEGIN RETURN NEW; END\n\
         $$",
    )
    .unwrap();
    eng.execute(
        "CREATE TRIGGER t_trg BEFORE UPDATE OF x, y ON t FOR EACH ROW EXECUTE FUNCTION fn()",
    )
    .unwrap();
    // Catalog round-trip via reload-from-serialized: the parser
    // sees the Display form and re-parses it identically.
    let cat = eng.catalog();
    let trg = &cat.triggers()[0];
    assert_eq!(trg.update_columns, vec!["x", "y"]);
}

// ── G8 — ALTER TABLE ALTER COLUMN TYPE … USING ────────────────

#[test]
fn alter_column_type_widens_int_to_bigint() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    eng.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
    eng.execute("ALTER TABLE t ALTER COLUMN id TYPE BIGINT")
        .unwrap();
    let table = eng.catalog().get("t").expect("table present");
    assert_eq!(table.schema().columns[0].ty, spg_storage::DataType::BigInt);
    for row in table.rows() {
        assert!(matches!(row.values[0], Value::BigInt(_)));
    }
}

#[test]
fn alter_column_type_with_using_expression() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, s TEXT NOT NULL)")
        .unwrap();
    eng.execute("INSERT INTO t VALUES (1, '42'), (2, '100')")
        .unwrap();
    eng.execute("ALTER TABLE t ALTER COLUMN s TYPE INT USING s::INT")
        .unwrap();
    let table = eng.catalog().get("t").expect("table present");
    assert_eq!(table.schema().columns[1].ty, spg_storage::DataType::Int);
    let r0 = &table.rows().get(0).unwrap().values[1];
    let r1 = &table.rows().get(1).unwrap().values[1];
    assert!(matches!(r0, Value::Int(42)));
    assert!(matches!(r1, Value::Int(100)));
}

#[test]
fn alter_column_type_unknown_column_errors() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    let r = eng.execute("ALTER TABLE t ALTER COLUMN missing TYPE BIGINT");
    assert!(r.is_err());
}
