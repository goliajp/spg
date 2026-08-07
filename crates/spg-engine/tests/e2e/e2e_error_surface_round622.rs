//! v7.39 (round 622, S05a) — what an error says, and which table a row is in.
//!
//! Two things the user sees, both of them SPG's own vocabulary leaking out.
//!
//! **The type in a message.** 421 sites printed `Value::data_type()` with
//! `{:?}`, so `upper(1)` answered "got Some(Int)" — Rust's Debug for an
//! Option wrapping an internal enum. `pg_type_name_for_error` already
//! existed; nothing but the `Option` was in the way. Against live PG18 the
//! names now agree even where the sentences do not:
//!
//!     upper(1)              SPG "got integer"     PG "upper(integer)"
//!     lower(ARRAY[1,2])     SPG "got integer[]"   PG "lower(integer[])"
//!     abs('x'::TEXT)        SPG "got text"        PG "abs(text)"
//!
//! **The table a row is in.** A partition parent is read through a synthetic
//! CTE, and `tableoid` resolved against THAT: every row of every child
//! reported `__spg_partition_pm`, a name no user ever typed. It was not
//! cosmetic — `WHERE tableoid::regclass::TEXT = 'pm_a'`, which is how one
//! asks "which partition holds this row", answered 0 rows where PG answers
//! 1. `ctid` had the same shape: it numbered the CTE's output, so rows in
//! different children got distinct ctids instead of each child's own
//! physical position. All shapes below were checked against live PG18.
//!
//! Recorded, not fixed, and found while probing this: `DROP TABLE <parent>
//! CASCADE` is still rejected ("drop the children first"), and SPG accepts
//! several calls PG rejects outright (`btrim(1)`, `replace(1,2,3)`,
//! `1 || ARRAY[1]`) — a coercion-strictness gap, not an error-surface one.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(err) => err.to_string(),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

/// The type a message names is PG's name for it, not Rust's Debug.
#[test]
fn round622_error_messages_name_pg_types() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT upper(1)", "integer"),
        ("SELECT lower(ARRAY[1,2])", "integer[]"),
        ("SELECT abs('x'::TEXT)", "text"),
        ("SELECT round('x'::TEXT)", "text"),
        ("SELECT bool_and(1)", "integer"),
        ("SELECT unnest(1)", "integer"),
        ("SELECT date_trunc('day', 1)", "integer"),
    ] {
        let m = err(&mut e, sql);
        assert!(
            m.contains(&alloc_fmt(want)),
            "{sql}: message should name the type as {want:?}, said {m:?}"
        );
        assert!(
            !m.contains("Some(") && !m.contains("None"),
            "{sql}: an Option leaked into the message: {m:?}"
        );
    }
}

fn alloc_fmt(s: &str) -> String {
    format!("got {s}")
}

/// A cast with no path says what it failed to cast — not which column.
#[test]
fn round622_failed_cast_is_not_phrased_as_a_column() {
    let mut e = Engine::new();
    // This one reached the INSERT-time column coercion and reported
    // `type mismatch in column "inet" (position 0)`, naming a column that
    // does not exist at a position that means nothing.
    let m = err(&mut e, "SELECT 1::INET");
    assert_eq!(m, "eval: type mismatch: cannot cast integer to inet", "{m}");
    let m = err(&mut e, "SELECT TRUE::TIMESTAMP");
    assert!(m.contains("cannot cast boolean to"), "{m}");
    // An INSERT still names the column — that phrasing belongs there.
    e.execute("CREATE TABLE ci (a INET)").unwrap();
    let m = err(&mut e, "INSERT INTO ci VALUES (1)");
    assert!(
        m.contains("column") && m.contains('a'),
        "an INSERT should still say which column: {m}"
    );
}

/// `tableoid` and `ctid` on a partition parent belong to the CHILD.
#[test]
fn round622_partition_parent_system_columns_are_the_childs() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE pm (id INT, g INT) PARTITION BY RANGE (id)")
        .unwrap();
    e.execute("CREATE TABLE pm_a PARTITION OF pm FOR VALUES FROM (0) TO (10)")
        .unwrap();
    e.execute("CREATE TABLE pm_b PARTITION OF pm FOR VALUES FROM (10) TO (20)")
        .unwrap();
    e.execute("INSERT INTO pm VALUES (1,1),(5,3),(11,2)")
        .unwrap();

    assert_eq!(
        vals(
            &mut e,
            "SELECT id, tableoid::regclass::TEXT FROM pm ORDER BY id"
        ),
        vec!["1|pm_a", "5|pm_a", "11|pm_b"],
        "each row names the child it lives in, never the synthetic CTE"
    );
    assert_eq!(
        vals(&mut e, "SELECT id, ctid FROM pm ORDER BY id"),
        vec!["1|(0,1)", "5|(0,2)", "11|(0,1)"],
        "ctid restarts per child — it is the child's physical position"
    );
    // The idiom this was silently emptying.
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM pm WHERE tableoid::regclass::TEXT = 'pm_a'"
        ),
        vec!["2"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT tableoid::regclass::TEXT, count(*) FROM pm GROUP BY 1 ORDER BY 1"
        ),
        vec!["pm_a|2", "pm_b|1"],
        "grouping by partition, which is what the idiom is usually for"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, tableoid::regclass::TEXT FROM pm WHERE id < 10 ORDER BY id"
        ),
        vec!["1|pm_a", "5|pm_a"],
        "and it survives the child prune, which drops pm_b from the union"
    );
    // A plain scan must be untouched: the system columns are carried only
    // when the statement asks for one, so `*` still yields the user columns.
    assert_eq!(
        vals(&mut e, "SELECT * FROM pm ORDER BY id"),
        vec!["1|1", "5|3", "11|2"]
    );
    assert_eq!(vals(&mut e, "SELECT count(*) FROM pm"), vec!["3"]);
    // Reading a child directly was always right, and stays right.
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, tableoid::regclass::TEXT FROM pm_a ORDER BY id"
        ),
        vec!["1|pm_a", "5|pm_a"]
    );
}
