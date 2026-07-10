//! v7.6.0 — parser-level coverage for FOREIGN KEY syntax. The
//! engine wiring lands in v7.6.1+; this file pins the AST shape so
//! later phases can rely on it.

use spg_sql::ast::{FkAction, ForeignKeyConstraint, MatchType, Statement};
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
    let fks =
        parse_create_table("CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL REFERENCES u(id))");
    assert_eq!(fks.len(), 1);
    let fk = &fks[0];
    assert_eq!(fk.name, None);
    assert_eq!(fk.columns, vec!["uid"]);
    assert_eq!(fk.parent_table, "u");
    assert_eq!(fk.parent_columns, vec!["id"]);
    // PG's default referential action with no ON DELETE/UPDATE clause is
    // NO ACTION (not RESTRICT).
    assert_eq!(fk.on_delete, FkAction::NoAction);
    assert_eq!(fk.on_update, FkAction::NoAction);
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
    // No ON UPDATE clause → PG default NO ACTION.
    assert_eq!(fks[0].on_update, FkAction::NoAction);
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
    let sql_in =
        "CREATE TABLE o (uid INT NOT NULL, FOREIGN KEY (uid) REFERENCES u(id) ON DELETE CASCADE)";
    let stmt = parse_statement(sql_in).unwrap();
    let rendered = format!("{stmt}");
    // Round-trip through parser — guarantees the WAL replay path
    // can reconstruct any FK shape we accept.
    let stmt2 = parse_statement(&rendered)
        .unwrap_or_else(|e| panic!("re-parse failed for {rendered:?}: {e:?}"));
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

// read01 A-group U25 — the optional `MATCH {SIMPLE|FULL|PARTIAL}`
// clause. SPG implements MATCH SIMPLE semantics; that spelling (the
// default, and the only one pg_dump emits) parses as a no-op, while
// FULL / PARTIAL are honestly rejected instead of silently applying
// SIMPLE (PG itself errors on MATCH PARTIAL as "not yet implemented").

#[test]
fn match_simple_table_level_accepted() {
    let fks = parse_create_table(
        "CREATE TABLE c (a INT, b INT, \
         FOREIGN KEY (a, b) REFERENCES p (a, b) MATCH SIMPLE)",
    );
    assert_eq!(fks.len(), 1);
    assert_eq!(fks[0].columns, vec!["a", "b"]);
    assert_eq!(fks[0].parent_table, "p");
}

#[test]
fn match_simple_column_level_accepted() {
    let fks = parse_create_table("CREATE TABLE c (x INT REFERENCES q(id) MATCH SIMPLE)");
    assert_eq!(fks.len(), 1);
    assert_eq!(fks[0].parent_table, "q");
}

#[test]
fn match_simple_before_on_delete() {
    // MATCH precedes the ON / DEFERRABLE trailers in PG's grammar.
    let fks = parse_create_table(
        "CREATE TABLE c (a INT, b INT, \
         FOREIGN KEY (a, b) REFERENCES p (a, b) MATCH SIMPLE ON DELETE CASCADE)",
    );
    assert_eq!(fks[0].on_delete, FkAction::Cascade);
}

#[test]
fn match_full_single_column_is_accepted() {
    // v7.38 (read01 P6.44) — MATCH FULL on a single-column FK is identical to
    // MATCH SIMPLE, so it parses (PG accepts it too).
    parse_statement("CREATE TABLE c (x INT REFERENCES q(id) MATCH FULL)")
        .expect("single-column MATCH FULL should parse");
}

#[test]
fn match_full_multi_column_parses() {
    // v7.38 (T29) — multi-column MATCH FULL is accepted (PG accepts it too) and
    // carries MatchType::Full through the AST; the engine enforces the
    // all-or-none-NULL rule. This test used to assert a parser rejection, which
    // T29 made obsolete.
    let fks = parse_create_table(
        "CREATE TABLE c (a INT, b INT, FOREIGN KEY (a, b) REFERENCES p (a, b) MATCH FULL)",
    );
    assert_eq!(fks.len(), 1);
    assert_eq!(fks[0].match_type, MatchType::Full);
    assert_eq!(fks[0].columns, ["a", "b"]);
}

#[test]
fn match_partial_is_rejected() {
    // PG18.4 rejects MATCH PARTIAL with exactly this wording.
    let err = parse_statement("CREATE TABLE c (x INT REFERENCES q(id) MATCH PARTIAL)")
        .expect_err("MATCH PARTIAL should be rejected");
    assert!(
        format!("{err:?}").contains("MATCH PARTIAL not yet implemented"),
        "got {err:?}"
    );
}
