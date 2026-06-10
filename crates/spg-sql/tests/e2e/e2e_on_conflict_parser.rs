//! v7.9.7 — parser coverage for ON CONFLICT clause shapes.

use spg_sql::ast::{OnConflictAction, Statement};
use spg_sql::parser::parse_statement;

fn parse_insert_on_conflict(sql: &str) -> spg_sql::ast::OnConflictClause {
    let stmt = parse_statement(sql).expect("parses");
    let Statement::Insert(ins) = stmt else {
        panic!("expected INSERT")
    };
    ins.on_conflict.expect("ON CONFLICT clause present")
}

#[test]
fn do_nothing_with_single_target() {
    let oc = parse_insert_on_conflict("INSERT INTO t VALUES (1) ON CONFLICT (id) DO NOTHING");
    assert_eq!(oc.target_columns, vec!["id"]);
    assert!(matches!(oc.action, OnConflictAction::Nothing));
}

#[test]
fn do_nothing_with_composite_target() {
    let oc = parse_insert_on_conflict("INSERT INTO t VALUES (1, 2) ON CONFLICT (a, b) DO NOTHING");
    assert_eq!(oc.target_columns, vec!["a", "b"]);
}

#[test]
fn do_nothing_without_target() {
    // mailrs idiom — let the engine pick the conflict index.
    let oc = parse_insert_on_conflict("INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING");
    assert!(oc.target_columns.is_empty());
    assert!(matches!(oc.action, OnConflictAction::Nothing));
}

#[test]
fn do_update_set_single_assignment() {
    let oc = parse_insert_on_conflict(
        "INSERT INTO accounts (address, password_hash) \
         VALUES ($1, $2) \
         ON CONFLICT (address) DO UPDATE SET password_hash = EXCLUDED.password_hash",
    );
    assert_eq!(oc.target_columns, vec!["address"]);
    let OnConflictAction::Update {
        assignments,
        where_,
    } = &oc.action
    else {
        panic!("expected Update")
    };
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].0, "password_hash");
    assert!(where_.is_none());
}

#[test]
fn do_update_set_multi_assignment() {
    let oc = parse_insert_on_conflict(
        "INSERT INTO calendar_events (uid, calendar_id, payload, etag) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (uid, calendar_id) DO UPDATE SET \
            payload = EXCLUDED.payload, \
            etag = EXCLUDED.etag",
    );
    let OnConflictAction::Update { assignments, .. } = &oc.action else {
        panic!()
    };
    assert_eq!(assignments.len(), 2);
}

#[test]
fn do_update_set_with_where_clause() {
    let oc = parse_insert_on_conflict(
        "INSERT INTO suppression_list (address, reason) \
         VALUES ($1, $2) \
         ON CONFLICT (address) DO UPDATE SET reason = EXCLUDED.reason \
         WHERE EXCLUDED.reason <> 'unknown'",
    );
    let OnConflictAction::Update { where_, .. } = &oc.action else {
        panic!()
    };
    assert!(where_.is_some());
}

#[test]
fn returning_after_on_conflict_works() {
    let stmt = parse_statement(
        "INSERT INTO t (k, v) VALUES (1, 'a') \
         ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v \
         RETURNING id, v",
    )
    .unwrap();
    let Statement::Insert(ins) = stmt else {
        panic!()
    };
    assert!(ins.on_conflict.is_some());
    assert!(ins.returning.is_some());
}

#[test]
fn missing_do_after_on_conflict_is_a_parse_error() {
    let r = parse_statement("INSERT INTO t VALUES (1) ON CONFLICT (id) NOTHING");
    assert!(r.is_err());
}

#[test]
fn unknown_action_after_do_is_a_parse_error() {
    let r = parse_statement("INSERT INTO t VALUES (1) ON CONFLICT (id) DO SOMETHING");
    assert!(r.is_err());
}
