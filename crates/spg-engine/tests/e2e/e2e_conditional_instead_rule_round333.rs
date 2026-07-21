//! read01 round 333 (V59) — conditional `DO INSTEAD <command>` rules.
//!
//! `CREATE RULE r AS ON UPDATE TO t WHERE old.id > 1 DO INSTEAD …` was
//! refused outright — a capability wall, not a wording gap: PG accepts the
//! rule and SPG answered "not yet implemented". Round 329 found it while
//! deparsing rule definitions.
//!
//! PG 18.4 measured. With `ON UPDATE TO r59 WHERE old.id > 1 DO INSTEAD
//! INSERT INTO log59 VALUES (old.id, new.v)` over rows (1,10) (2,20)
//! (3,30):
//!
//! ```text
//! UPDATE r59 SET v = 999;   -- UPDATE 1
//! r59  → (1,999) (2,20) (3,30)
//! log59 → (2,999) (3,999)
//! ```
//!
//! So the qualification splits the statement: the rows it holds for take
//! the substitute action, the rest run the original, and the command tag
//! counts only the latter.
//!
//! Both halves of the machinery already existed — the row-narrowing from
//! conditional `DO INSTEAD NOTHING` (round 141) and the per-row WHERE test
//! in the action runner (round 140). What was missing was letting a rule
//! use both at once.

use spg_engine::Engine;
use spg_storage::Value;

fn rows_of(e: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        spg_engine::QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| r.values.iter().cloned().map(Value::into_owned).collect())
            .collect(),
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

fn affected(e: &mut Engine, sql: &str) -> usize {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        spg_engine::QueryResult::CommandOk { affected, .. } => affected,
        other => panic!("`{sql}` did not return a command tag: {other:?}"),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE r59 (id INT, v INT)").unwrap();
    e.execute("CREATE TABLE log59 (id INT, v INT)").unwrap();
    e.execute("INSERT INTO r59 VALUES (1, 10), (2, 20), (3, 30)")
        .unwrap();
    e
}

#[test]
fn a_conditional_instead_command_rule_is_accepted() {
    let mut e = fixture();
    e.execute(
        "CREATE RULE rcond AS ON UPDATE TO r59 WHERE old.id > 1 \
         DO INSTEAD INSERT INTO log59 VALUES (old.id, new.v)",
    )
    .expect("PG accepts this rule");
}

/// The qualification splits the statement, exactly as PG's does.
#[test]
fn the_matching_rows_take_the_action_and_the_rest_run_the_original() {
    let mut e = fixture();
    e.execute(
        "CREATE RULE rcond AS ON UPDATE TO r59 WHERE old.id > 1 \
         DO INSTEAD INSERT INTO log59 VALUES (old.id, new.v)",
    )
    .unwrap();

    assert_eq!(
        affected(&mut e, "UPDATE r59 SET v = 999"),
        1,
        "only the row the condition misses is updated — PG reports UPDATE 1"
    );
    assert_eq!(
        rows_of(&mut e, "SELECT id, v FROM r59 ORDER BY id"),
        vec![
            vec![Value::Int(1), Value::Int(999)],
            vec![Value::Int(2), Value::Int(20)],
            vec![Value::Int(3), Value::Int(30)],
        ],
    );
    assert_eq!(
        rows_of(&mut e, "SELECT id, v FROM log59 ORDER BY id"),
        vec![
            vec![Value::Int(2), Value::Int(999)],
            vec![Value::Int(3), Value::Int(999)],
        ],
        "the claimed rows produced the substitute action, with NEW bound"
    );
}

/// An UNCONDITIONAL instead-command rule still replaces the whole
/// statement — the behaviour this round must not disturb.
#[test]
fn an_unconditional_instead_command_still_replaces_everything() {
    let mut e = fixture();
    e.execute(
        "CREATE RULE runc AS ON UPDATE TO r59 \
         DO INSTEAD INSERT INTO log59 VALUES (old.id, new.v)",
    )
    .unwrap();
    assert_eq!(affected(&mut e, "UPDATE r59 SET v = 999"), 0);
    assert_eq!(
        rows_of(&mut e, "SELECT count(*) FROM log59"),
        vec![vec![Value::BigInt(3)]],
        "every row went to the action"
    );
    assert_eq!(
        rows_of(&mut e, "SELECT count(*) FROM r59 WHERE v = 999"),
        vec![vec![Value::BigInt(0)]],
        "and none was updated"
    );
}

/// Conditional `DO INSTEAD NOTHING` — the neighbouring form — is unchanged.
#[test]
fn a_conditional_instead_nothing_still_narrows() {
    let mut e = fixture();
    e.execute("CREATE RULE rdel AS ON DELETE TO r59 WHERE old.id = 1 DO INSTEAD NOTHING")
        .unwrap();
    assert_eq!(affected(&mut e, "DELETE FROM r59"), 2);
    assert_eq!(
        rows_of(&mut e, "SELECT id FROM r59 ORDER BY id"),
        vec![vec![Value::Int(1)]],
        "the protected row survives"
    );
}

/// A conditional DELETE rule claims its rows the same way.
#[test]
fn a_conditional_instead_command_on_delete_splits_too() {
    let mut e = fixture();
    e.execute(
        "CREATE RULE rdc AS ON DELETE TO r59 WHERE old.id = 3 \
         DO INSTEAD INSERT INTO log59 VALUES (old.id, old.v)",
    )
    .unwrap();
    assert_eq!(
        affected(&mut e, "DELETE FROM r59"),
        2,
        "rows 1 and 2 are deleted; row 3 took the action instead"
    );
    assert_eq!(
        rows_of(&mut e, "SELECT id, v FROM r59 ORDER BY id"),
        vec![vec![Value::Int(3), Value::Int(30)]],
    );
    assert_eq!(
        rows_of(&mut e, "SELECT id, v FROM log59"),
        vec![vec![Value::Int(3), Value::Int(30)]],
    );
}

/// And it round-trips through `pg_get_ruledef` with the qualification on
/// its own line, as round 329 established.
#[test]
fn the_definition_reflects_the_qualification() {
    let mut e = fixture();
    e.execute(
        "CREATE RULE rcond AS ON UPDATE TO r59 WHERE old.id > 1 \
         DO INSTEAD INSERT INTO log59 VALUES (old.id, new.v)",
    )
    .unwrap();
    let def = match rows_of(
        &mut e,
        "SELECT pg_get_ruledef(oid) FROM pg_rewrite WHERE rulename = 'rcond'",
    )
    .into_iter()
    .next()
    {
        Some(mut r) => match r.remove(0) {
            Value::Text(t) => t.to_string(),
            other => panic!("{other:?}"),
        },
        None => panic!("no ruledef"),
    };
    assert!(
        def.contains("\n   WHERE (old.id > 1) DO INSTEAD  INSERT INTO log59 (id, v)"),
        "the qualification rides its own line: {def}"
    );
}
