//! v7.39 (round 525) — the bare-context sweep.
//!
//! The same shape has now been found by accident three rounds running:
//! a subsystem that builds its own evaluation context and leaves the
//! session out of it (round 522's temp engine, round 523's literal
//! walker, round 524's write paths). So this round enumerated them
//! instead of waiting for the fourth: 57 contexts, 49 of them bare.
//!
//! Most are internal — a synthesised aggregate schema, an index walk —
//! and cannot observe a session. Six could, and every one of them was
//! measured against PG18 before and after:
//!
//!     CHECK (a = current_setting('app.tenant'))   PG accepts   SPG ERROR
//!     ON CONFLICT DO UPDATE SET w = …             PG acme      SPG unchanged
//!     JOIN … WHERE t = current_setting(…)         PG 1 row     SPG ERROR
//!     MERGE … WHEN MATCHED THEN UPDATE SET …      PG acme      SPG ERROR
//!     DEFAULT current_setting('app.tenant')       PG acme      SPG ERROR
//!     (aggregate and FOR UPDATE predicates already worked)
//!
//! A dotted GUC is what makes these visible: it lives only in the
//! session, so its absence ERRORS. A built-in name like `current_user`
//! has a fallback and answers the wrong thing quietly instead — which is
//! how the first three rounds' bugs stayed hidden.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("SET app.tenant = 'acme'").unwrap();
    e
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .unwrap_or_default(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// A CHECK may name a session setting, and PG evaluates it in the
/// session doing the writing. Without one the INSERT failed outright.
#[test]
fn round525_check_constraint_sees_the_session() {
    let mut e = engine();
    e.execute("CREATE TABLE c1 (a TEXT CHECK (a = current_setting('app.tenant')))")
        .unwrap();
    e.execute("INSERT INTO c1 VALUES ('acme')").unwrap();
    assert_eq!(text(&mut e, "SELECT a FROM c1"), "acme");
    // And it still REJECTS a row that fails the check.
    assert!(e.execute("INSERT INTO c1 VALUES ('other')").is_err());
}

/// The upsert branch takes the same session the plain assignment does.
#[test]
fn round525_on_conflict_update_sees_the_session() {
    let mut e = engine();
    e.execute("CREATE TABLE c2 (id INT PRIMARY KEY, who TEXT)")
        .unwrap();
    e.execute("INSERT INTO c2 VALUES (1, 'x')").unwrap();
    e.execute(
        "INSERT INTO c2 VALUES (1, 'y') ON CONFLICT (id) \
         DO UPDATE SET who = current_setting('app.tenant')",
    )
    .unwrap();
    assert_eq!(text(&mut e, "SELECT who FROM c2"), "acme");
}

/// A join's WHERE is the same predicate the unjoined shape carries — it
/// failed on one and worked on the other.
#[test]
fn round525_joined_where_sees_the_session() {
    let mut e = engine();
    e.execute("CREATE TABLE j1 (a INT, t TEXT)").unwrap();
    e.execute("CREATE TABLE j2 (a INT)").unwrap();
    e.execute("INSERT INTO j1 VALUES (1, 'acme'), (2, 'other')")
        .unwrap();
    e.execute("INSERT INTO j2 VALUES (1), (2)").unwrap();
    let joined = text(
        &mut e,
        "SELECT count(*) FROM j1 JOIN j2 ON j1.a = j2.a \
         WHERE j1.t = current_setting('app.tenant')",
    );
    let plain = text(
        &mut e,
        "SELECT count(*) FROM j1 WHERE t = current_setting('app.tenant')",
    );
    assert_eq!(joined, "1");
    assert_eq!(joined, plain);
}

/// MERGE's clauses read it too.
#[test]
fn round525_merge_sees_the_session() {
    let mut e = engine();
    e.execute("CREATE TABLE m1 (id INT, v TEXT)").unwrap();
    e.execute("CREATE TABLE m2 (id INT)").unwrap();
    e.execute("INSERT INTO m1 VALUES (1, 'z')").unwrap();
    e.execute("INSERT INTO m2 VALUES (1)").unwrap();
    e.execute(
        "MERGE INTO m1 USING m2 ON m1.id = m2.id \
         WHEN MATCHED THEN UPDATE SET v = current_setting('app.tenant')",
    )
    .unwrap();
    assert_eq!(text(&mut e, "SELECT v FROM m1"), "acme");
}

/// PG takes any expression as a DEFAULT. SPG accepted eight names and
/// refused everything else, so `DEFAULT current_setting(…)`,
/// `DEFAULT upper(…)` and `DEFAULT 2 * 3` all failed the INSERT. The
/// eight are now a fast path that skips a parse per row, not the list.
#[test]
fn round525_runtime_default_takes_any_expression() {
    let mut e = engine();
    for (i, (decl, expect)) in [
        ("w TEXT DEFAULT current_setting('app.tenant')", "acme"),
        ("w TEXT DEFAULT upper('ab')", "AB"),
        ("w INT DEFAULT 2 * 3", "6"),
        ("w TEXT DEFAULT ('x' || 'y')", "xy"),
    ]
    .into_iter()
    .enumerate()
    {
        let t = format!("dd{i}");
        e.execute(&format!("CREATE TABLE {t} (a INT, {decl})"))
            .unwrap();
        e.execute(&format!("INSERT INTO {t} (a) VALUES (1)"))
            .unwrap();
        assert_eq!(
            text(&mut e, &format!("SELECT w::text FROM {t}")),
            expect,
            "{decl}"
        );
    }
    // The fast-path names still work.
    e.execute("CREATE TABLE dn (a INT, w TIMESTAMP DEFAULT now())")
        .unwrap();
    e.execute("INSERT INTO dn (a) VALUES (1)").unwrap();
    assert_eq!(
        text(&mut e, "SELECT count(*) FROM dn WHERE w IS NOT NULL"),
        "1"
    );
}

/// The two that already worked, pinned so a later change that breaks
/// them is not mistaken for something else.
#[test]
fn round525_the_two_that_already_worked() {
    let mut e = engine();
    e.execute("CREATE TABLE k (a INT, t TEXT)").unwrap();
    e.execute("INSERT INTO k VALUES (1, 'acme'), (2, 'other')")
        .unwrap();
    assert_eq!(
        text(
            &mut e,
            "SELECT sum(a) FROM k WHERE t = current_setting('app.tenant')"
        ),
        "1"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT count(*) FROM (SELECT a FROM k \
             WHERE t = current_setting('app.tenant') FOR UPDATE) z"
        ),
        "1"
    );
}
