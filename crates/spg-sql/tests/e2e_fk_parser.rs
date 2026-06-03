//! v7.6.0 — parser-level coverage for FOREIGN KEY syntax. The
//! engine wiring lands in v7.6.1+; this file pins the AST shape so
//! later phases can rely on it.

use spg_sql::ast::{FkAction, ForeignKeyConstraint, Statement};
use spg_sql::parser::parse_statement;

fn parse_create_table(sql: &str) -> Vec<ForeignKeyConstraint> {
    match parse_statement(sql).expect("parses") {
        Statement::CreateTable(t) => t.foreign_keys,
        other => panic!("expected CREATE TABLE, got {other:?}"),
    }
}

#[test]
fn no_fk_means_empty_vec() {
    let fks = parse_create_table("CREATE TABLE u (id INT NOT NULL, name TEXT)");
    assert!(fks.is_empty());
}

#[test]
fn column_level_references_normalises_to_table_level() {
    let fks = parse_create_table(
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL REFERENCES u(id))",
    );
    assert_eq!(fks.len(), 1);
    let fk = &fks[0];
    assert_eq!(fk.name, None);
    assert_eq!(fk.columns, vec!["uid"]);
    assert_eq!(fk.parent_table, "u");
    assert_eq!(fk.parent_columns, vec!["id"]);
    assert_eq!(fk.on_delete, FkAction::Restrict);
    assert_eq!(fk.on_update, FkAction::Restrict);
}

#[test]
fn table_level_foreign_key_basic() {
    let fks = parse_create_table(
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL, FOREIGN KEY (uid) REFERENCES u(id))",
    );
    assert_eq!(fks.len(), 1);
    assert_eq!(fks[0].columns, vec!["uid"]);
    assert_eq!(fks[0].parent_table, "u");
    assert_eq!(fks[0].parent_columns, vec!["id"]);
}

#[test]
fn table_level_with_constraint_name() {
    let fks = parse_create_table(
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL, \
         CONSTRAINT fk_user FOREIGN KEY (uid) REFERENCES u(id))",
    );
    assert_eq!(fks[0].name, Some("fk_user".into()));
}

#[test]
fn on_delete_cascade() {
    let fks = parse_create_table(
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL, \
         FOREIGN KEY (uid) REFERENCES u(id) ON DELETE CASCADE)",
    );
    assert_eq!(fks[0].on_delete, FkAction::Cascade);
    assert_eq!(fks[0].on_update, FkAction::Restrict);
}

#[test]
fn on_delete_set_null_set_default_no_action() {
    let cases = [
        ("ON DELETE SET NULL", FkAction::SetNull),
        ("ON DELETE SET DEFAULT", FkAction::SetDefault),
        ("ON DELETE NO ACTION", FkAction::NoAction),
        ("ON DELETE RESTRICT", FkAction::Restrict),
    ];
    for (clause, want) in cases {
        let sql = format!(
            "CREATE TABLE o (uid INT NOT NULL, FOREIGN KEY (uid) REFERENCES u(id) {clause})"
        );
        let fks = parse_create_table(&sql);
        assert_eq!(fks[0].on_delete, want, "clause = {clause}");
    }
}

#[test]
fn on_delete_and_on_update_combined_either_order() {
    let fks = parse_create_table(
        "CREATE TABLE o (uid INT NOT NULL, \
         FOREIGN KEY (uid) REFERENCES u(id) ON DELETE CASCADE ON UPDATE SET NULL)",
    );
    assert_eq!(fks[0].on_delete, FkAction::Cascade);
    assert_eq!(fks[0].on_update, FkAction::SetNull);

    // Reverse order parses the same.
    let fks = parse_create_table(
        "CREATE TABLE o (uid INT NOT NULL, \
         FOREIGN KEY (uid) REFERENCES u(id) ON UPDATE SET NULL ON DELETE CASCADE)",
    );
    assert_eq!(fks[0].on_delete, FkAction::Cascade);
    assert_eq!(fks[0].on_update, FkAction::SetNull);
}

#[test]
fn composite_fk_multiple_columns() {
    let fks = parse_create_table(
        "CREATE TABLE child (a INT NOT NULL, b INT NOT NULL, \
         FOREIGN KEY (a, b) REFERENCES parent(x, y))",
    );
    assert_eq!(fks[0].columns, vec!["a", "b"]);
    assert_eq!(fks[0].parent_columns, vec!["x", "y"]);
}

#[test]
fn arity_mismatch_is_rejected() {
    let r = parse_statement(
        "CREATE TABLE child (a INT NOT NULL, b INT NOT NULL, \
         FOREIGN KEY (a, b) REFERENCES parent(x))",
    );
    assert!(r.is_err(), "arity mismatch must be a parse error");
}

#[test]
fn repeated_on_delete_is_rejected() {
    let r = parse_statement(
        "CREATE TABLE o (uid INT NOT NULL, FOREIGN KEY (uid) REFERENCES u(id) \
         ON DELETE CASCADE ON DELETE RESTRICT)",
    );
    assert!(r.is_err());
}

#[test]
fn display_round_trips_simple_fk() {
    let sql_in = "CREATE TABLE o (uid INT NOT NULL, FOREIGN KEY (uid) REFERENCES u(id) ON DELETE CASCADE)";
    let stmt = parse_statement(sql_in).unwrap();
    let rendered = format!("{stmt}");
    // Round-trip through parser — guarantees the WAL replay path
    // can reconstruct any FK shape we accept.
    let stmt2 = parse_statement(&rendered).unwrap_or_else(|e| {
        panic!("re-parse failed for {rendered:?}: {e:?}")
    });
    assert_eq!(stmt, stmt2);
}

#[test]
fn multiple_fks_in_one_table() {
    let fks = parse_create_table(
        "CREATE TABLE o (a INT NOT NULL, b INT NOT NULL, \
         FOREIGN KEY (a) REFERENCES p1(id), \
         FOREIGN KEY (b) REFERENCES p2(id) ON DELETE CASCADE)",
    );
    assert_eq!(fks.len(), 2);
    assert_eq!(fks[0].parent_table, "p1");
    assert_eq!(fks[1].parent_table, "p2");
    assert_eq!(fks[1].on_delete, FkAction::Cascade);
}
