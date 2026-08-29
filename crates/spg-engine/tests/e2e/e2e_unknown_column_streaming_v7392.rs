//! v7.39.2 — the streaming SELECT entry refuses an unknown column too.
//!
//! `execute_readonly_select_streaming` runs BELOW `exec_select_cancel`,
//! which is where the check for an unknown column in WHERE / ORDER BY /
//! GROUP BY / HAVING went first. On an empty table that route therefore
//! kept answering zero rows while the materialising one raised — and
//! the ACL check had been patched into the same entries, for the same
//! reason, in v7.39.
//!
//! Pinned here rather than on the wire because the wire reaches this
//! entry only when the statement fails to prepare, which a well-formed
//! SELECT never does; the engine tests call it directly.

use spg_engine::{CancelToken, Engine, StreamItem};

fn stream(e: &mut Engine, sql: &str) -> Result<usize, String> {
    let mut n = 0usize;
    e.execute_readonly_select_streaming(sql, CancelToken::none(), |item| {
        if matches!(item, StreamItem::Row(_)) {
            n += 1;
        }
        Ok(())
    })
    .map(|_| n)
    .map_err(|e| e.to_string())
}

#[test]
fn the_streaming_entry_refuses_an_unknown_column_on_an_empty_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE es (a int)").expect("create");
    for sql in [
        "SELECT a FROM es WHERE nosuch = 1",
        "SELECT a FROM es ORDER BY nosuch",
        "SELECT * FROM es WHERE nosuch = 1",
    ] {
        let got = stream(&mut e, sql);
        assert!(
            got.as_ref().is_err_and(|m| m.contains("nosuch")),
            "{sql}: {got:?}"
        );
    }
    // The control: names that resolve still stream, empty or not.
    assert_eq!(stream(&mut e, "SELECT a FROM es WHERE a = 1"), Ok(0));
    e.execute("INSERT INTO es VALUES (1)").expect("insert");
    assert_eq!(stream(&mut e, "SELECT a FROM es WHERE a = 1"), Ok(1));
}

/// v7.39.2 — PostgreSQL prints a QUALIFIED missing column unquoted and
/// dotted, and an unqualified one quoted. Measured on PG 18.6:
///
///   SELECT … WHERE ea.no_such = 1   column ea.no_such does not exist
///   SELECT … WHERE no_such = 1      column "no_such" does not exist
///
/// The clause validator raised the bare form for both, which drops the
/// alias a caller matches on — and that is exactly what a caller does:
/// the sqlx round-20 pin asserts the message names `ea.no_such_column`,
/// and it went red in CI while every local gate stayed green, because
/// the local tier runs spg-sqlx's lib tests and not its e2e ones.
#[test]
fn a_qualified_missing_column_keeps_its_alias() {
    let mut e = spg_engine::Engine::new();
    e.execute("CREATE TABLE q1 (id INT, thread INT)").unwrap();
    e.execute("CREATE TABLE q2 (mid INT, flag BOOL)").unwrap();
    // The engine's own Display carries an `eval: ` prefix that the wire
    // strips; what is pinned here is the sentence after it.
    let msg = |e: &mut spg_engine::Engine, sql: &str| {
        format!("{}", e.execute(sql).unwrap_err())
            .trim_start_matches("eval: ")
            .to_string()
    };
    assert_eq!(
        msg(
            &mut e,
            "SELECT COUNT(*) FROM q1 m LEFT JOIN q2 ea ON ea.mid = m.id \
             GROUP BY m.thread HAVING BOOL_OR(ea.no_such_column)"
        ),
        "column ea.no_such_column does not exist"
    );
    assert_eq!(
        msg(&mut e, "SELECT id FROM q1 m WHERE m.nope = 1"),
        "column m.nope does not exist"
    );
    // The unqualified twin keeps PostgreSQL's quotes.
    assert_eq!(
        msg(&mut e, "SELECT id FROM q1 m WHERE nope = 1"),
        "column \"nope\" does not exist"
    );
}
