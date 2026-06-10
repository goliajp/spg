//! v7.9.18 — parser coverage for table-level UNIQUE / PRIMARY KEY
//! clauses. mailrs migration follow-up G1 + G6.

use spg_sql::ast::{Statement, TableConstraint};
use spg_sql::parser::parse_statement;

fn create_table_constraints(sql: &str) -> Vec<TableConstraint> {
    match parse_statement(sql).expect("parses") {
        Statement::CreateTable(t) => t.table_constraints,
        other => panic!("expected CREATE TABLE, got {other:?}"),
    }
}

#[test]
fn table_level_unique_single_column_parses() {
    let cs = create_table_constraints(
        "CREATE TABLE u (id INT NOT NULL, address TEXT NOT NULL, UNIQUE (address))",
    );
    assert_eq!(cs.len(), 1);
    let TableConstraint::Unique { columns, .. } = &cs[0] else {
        panic!()
    };
    assert_eq!(columns, &vec!["address".to_string()]);
}

#[test]
fn table_level_unique_composite_parses() {
    // mailrs `messages` IMAP UID uniqueness pattern.
    let cs = create_table_constraints(
        "CREATE TABLE messages (\
            id BIGSERIAL,\
            mailbox_id BIGINT NOT NULL,\
            uid INTEGER NOT NULL,\
            UNIQUE (mailbox_id, uid)\
        )",
    );
    assert_eq!(cs.len(), 1);
    let TableConstraint::Unique { columns, .. } = &cs[0] else {
        panic!()
    };
    assert_eq!(columns.len(), 2);
}

#[test]
fn composite_primary_key_parses() {
    // mailrs snoozed_conversations pattern.
    let cs = create_table_constraints(
        "CREATE TABLE snoozed (\
            thread_id TEXT NOT NULL,\
            account_address TEXT NOT NULL,\
            PRIMARY KEY (thread_id, account_address)\
        )",
    );
    assert_eq!(cs.len(), 1);
    let TableConstraint::PrimaryKey { columns, .. } = &cs[0] else {
        panic!()
    };
    assert_eq!(
        columns,
        &vec!["thread_id".to_string(), "account_address".to_string()]
    );
}

#[test]
fn table_with_both_pk_and_unique_constraints() {
    let cs = create_table_constraints(
        "CREATE TABLE t (\
            a INT NOT NULL,\
            b INT NOT NULL,\
            c INT NOT NULL,\
            PRIMARY KEY (a),\
            UNIQUE (b, c)\
        )",
    );
    assert_eq!(cs.len(), 2);
}

#[test]
fn table_constraints_interleaved_with_inline_fk() {
    // PRIMARY KEY at end after inline FOREIGN KEY clause.
    let stmt = parse_statement(
        "CREATE TABLE child (\
            id INT NOT NULL,\
            parent_id INT NOT NULL,\
            extra INT NOT NULL,\
            FOREIGN KEY (parent_id) REFERENCES parent(id),\
            UNIQUE (id, extra)\
        )",
    )
    .unwrap();
    let Statement::CreateTable(t) = stmt else {
        panic!()
    };
    assert_eq!(t.foreign_keys.len(), 1);
    assert_eq!(t.table_constraints.len(), 1);
}

#[test]
fn unique_without_parens_is_rejected() {
    let r = parse_statement("CREATE TABLE t (a INT NOT NULL, UNIQUE a)");
    assert!(r.is_err());
}

#[test]
fn primary_key_without_columns_is_rejected() {
    let r = parse_statement("CREATE TABLE t (a INT NOT NULL, PRIMARY KEY ())");
    assert!(r.is_err());
}

#[test]
fn table_level_unique_after_many_columns() {
    // Stress: 5 columns + composite unique on the last 2.
    let cs = create_table_constraints(
        "CREATE TABLE t (\
            a INT NOT NULL,\
            b INT NOT NULL,\
            c INT NOT NULL,\
            d INT NOT NULL,\
            e INT NOT NULL,\
            UNIQUE (d, e)\
        )",
    );
    let TableConstraint::Unique { columns, .. } = &cs[0] else {
        panic!()
    };
    assert_eq!(columns, &vec!["d".to_string(), "e".to_string()]);
}
