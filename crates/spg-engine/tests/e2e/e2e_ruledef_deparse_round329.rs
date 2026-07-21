//! read01 round 329 (V47) — `pg_get_ruledef` deparses the action.
//!
//! SPG echoed `RuleDef.commands` — the text the user typed. PG re-deparses
//! the action from its parse tree, which changes three things, all
//! measured on PG 18.4:
//!
//!   * an INSERT always carries an explicit column list, filled in when
//!     the user omitted it: `INSERT INTO r33 (id, v)`;
//!   * `VALUES` / `WHERE` start a new line indented by two;
//!   * a WHERE column with no qualifier is printed qualified by the target
//!     table (`WHERE (r33.id = old.id)`), which is how the deparser
//!     resolves it against the range table.
//!
//! PG's own output for the four shapes below, verbatim:
//!
//! ```text
//! CREATE RULE ra AS
//!     ON INSERT TO public.r33 DO  INSERT INTO log33 (id, v)
//!   VALUES (new.id, new.v);
//! CREATE RULE rb AS
//!     ON UPDATE TO public.r33 DO INSTEAD  UPDATE r33 SET v = new.v
//!   WHERE (r33.id = old.id);
//! CREATE RULE rc AS
//!     ON DELETE TO public.r33 DO  DELETE FROM log33
//!   WHERE (log33.id = old.id);
//! CREATE RULE r_del AS
//!     ON DELETE TO public.r33 DO INSTEAD NOTHING;
//! ```
//!
//! Note the spacing PG uses: an action is preceded by one further space
//! (`DO  INSERT`, `DO INSTEAD  UPDATE`) while `NOTHING` is not.

use spg_engine::Engine;
use spg_storage::Value;

fn ruledef(e: &mut Engine, name: &str) -> String {
    let sql = format!("SELECT pg_get_ruledef(oid) FROM pg_rewrite WHERE rulename = '{name}'");
    match e.execute(&sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        spg_engine::QueryResult::Rows { rows, .. } => match rows
            .first()
            .and_then(|r| r.values.first())
            .cloned()
            .map(Value::into_owned)
        {
            Some(Value::Text(t)) => t.to_string(),
            other => panic!("no ruledef for {name}: {other:?}"),
        },
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE r33 (id INT, v INT)").unwrap();
    e.execute("CREATE TABLE log33 (id INT, v INT)").unwrap();
    e.execute(
        "CREATE RULE ra AS ON INSERT TO r33 DO ALSO \
         INSERT INTO log33 (id, v) VALUES (new.id, new.v)",
    )
    .unwrap();
    e.execute(
        "CREATE RULE rb AS ON UPDATE TO r33 DO INSTEAD \
         UPDATE r33 SET v = new.v WHERE id = old.id",
    )
    .unwrap();
    e.execute("CREATE RULE rc AS ON DELETE TO r33 DO ALSO DELETE FROM log33 WHERE id = old.id")
        .unwrap();
    e.execute("CREATE RULE rd AS ON DELETE TO r33 DO INSTEAD NOTHING")
        .unwrap();
    e
}

#[test]
fn an_insert_action_is_deparsed_like_pgs() {
    let mut e = fixture();
    assert_eq!(
        ruledef(&mut e, "ra"),
        "CREATE RULE ra AS\n    ON INSERT TO public.r33 DO  \
         INSERT INTO log33 (id, v)\n  VALUES (new.id, new.v);",
    );
}

/// The column list is filled in from the catalog when the statement did
/// not name one — the case the ledger recorded.
#[test]
fn an_insert_without_a_column_list_gets_one() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE r33 (id INT, v INT)").unwrap();
    e.execute("CREATE RULE r_ins AS ON INSERT TO r33 DO ALSO INSERT INTO r33 VALUES (99, 99)")
        .unwrap();
    assert_eq!(
        ruledef(&mut e, "r_ins"),
        "CREATE RULE r_ins AS\n    ON INSERT TO public.r33 DO  \
         INSERT INTO r33 (id, v)\n  VALUES (99, 99);",
    );
}

#[test]
fn an_update_action_qualifies_its_where_columns() {
    let mut e = fixture();
    assert_eq!(
        ruledef(&mut e, "rb"),
        "CREATE RULE rb AS\n    ON UPDATE TO public.r33 DO INSTEAD  \
         UPDATE r33 SET v = new.v\n  WHERE (r33.id = old.id);",
    );
}

#[test]
fn a_delete_action_qualifies_against_its_own_target() {
    let mut e = fixture();
    assert_eq!(
        ruledef(&mut e, "rc"),
        "CREATE RULE rc AS\n    ON DELETE TO public.r33 DO  \
         DELETE FROM log33\n  WHERE (log33.id = old.id);",
    );
}

/// `NOTHING` is not an action and keeps PG's single space.
#[test]
fn do_instead_nothing_keeps_its_spacing() {
    let mut e = fixture();
    assert_eq!(
        ruledef(&mut e, "rd"),
        "CREATE RULE rd AS\n    ON DELETE TO public.r33 DO INSTEAD NOTHING;",
    );
}

/// The rules still fire — this is a reflection change, not a semantic one.
#[test]
fn the_rules_still_do_what_they_say() {
    let mut e = fixture();
    e.execute("INSERT INTO r33 VALUES (1, 10)").unwrap();
    match e.execute("SELECT count(*) FROM log33").unwrap() {
        spg_engine::QueryResult::Rows { rows, .. } => {
            assert_eq!(
                rows.first().and_then(|r| r.values.first()),
                Some(&Value::BigInt(1)),
                "the ON INSERT DO ALSO rule wrote its log row"
            );
        }
        other => panic!("{other:?}"),
    }
}
