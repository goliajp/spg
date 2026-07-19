//! v7.39 (round 267) — auto-updatable views: one judgement, honest to
//! both the write path and the reflection views.
//!
//! Before this round the two disagreed in both directions. `views`
//! reported is_updatable = NO for every view while INSERT/UPDATE/DELETE
//! through a simple view had worked since 19.13; and a write to a view
//! that genuinely is not auto-updatable reported `relation "na" does
//! not exist` about a view the catalog plainly has.
//!
//! Every expectation was read off live PG 18.4, including the reason
//! PRECEDENCE (views that break two rules at once) and the fact that
//! ORDER BY does not block updatability.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows from {sql}");
    };
    rows.into_iter()
        .map(|row| row.values.into_iter().collect())
        .collect()
}

fn lines(e: &mut Engine, sql: &str) -> Vec<String> {
    rows(e, sql)
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    spg_storage::Value::Null => String::new(),
                    other => spg_engine::eval::value_to_text(other),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(_) => panic!("{sql}: expected an error"),
        Err(x) => format!("{x}"),
    }
}

/// The full shape matrix, exactly as PG 18.4 answers it.
fn matrix_engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE nt (a int, b text)").unwrap();
    e.execute("CREATE TABLE nt2 (x int)").unwrap();
    e.execute("CREATE VIEW na AS SELECT b, count(*) AS c FROM nt GROUP BY b")
        .unwrap();
    e.execute("CREATE VIEW nj AS SELECT nt.a, nt2.x FROM nt JOIN nt2 ON nt.a = nt2.x")
        .unwrap();
    e.execute("CREATE VIEW nd AS SELECT DISTINCT a FROM nt").unwrap();
    e.execute("CREATE VIEW nl AS SELECT a FROM nt LIMIT 5").unwrap();
    e.execute("CREATE VIEW ne AS SELECT a, a + 1 AS a2 FROM nt")
        .unwrap();
    e.execute("CREATE VIEW nu AS SELECT a FROM nt UNION SELECT x FROM nt2")
        .unwrap();
    e.execute("CREATE VIEW nw AS SELECT a FROM nt WHERE a > 0").unwrap();
    e.execute("CREATE VIEW nv AS SELECT a FROM nw").unwrap();
    e
}

#[test]
fn is_updatable_matches_pg_across_every_shape() {
    let mut e = matrix_engine();
    // Live PG 18.4, verbatim. Note ne: an expression column does NOT
    // make the view non-updatable — only that one column is unwritable.
    // And nv shows a view over a view stays updatable ("a single table
    // or view").
    assert_eq!(
        lines(
            &mut e,
            "SELECT table_name, is_updatable, is_insertable_into \
             FROM information_schema.views \
             WHERE table_name IN ('na','nj','nd','nl','ne','nu','nw','nv') \
             ORDER BY table_name",
        ),
        vec![
            "na|NO|NO",
            "nd|NO|NO",
            "ne|YES|YES",
            "nj|NO|NO",
            "nl|NO|NO",
            "nu|NO|NO",
            "nv|YES|YES",
            "nw|YES|YES",
        ],
    );
}

#[test]
fn order_by_does_not_block_updatability() {
    // PG auto-updates a view with ORDER BY; SPG rejected it until this
    // round, so an INSERT PG accepts failed here.
    let mut e = Engine::new();
    e.execute("CREATE TABLE ot (a int)").unwrap();
    e.execute("CREATE VIEW ob AS SELECT a FROM ot ORDER BY a").unwrap();
    assert_eq!(
        lines(
            &mut e,
            "SELECT is_updatable FROM information_schema.views WHERE table_name = 'ob'",
        ),
        vec!["YES"],
    );
    e.execute("INSERT INTO ob VALUES (1)").unwrap();
    assert_eq!(lines(&mut e, "SELECT a FROM ot"), vec!["1"]);
}

#[test]
fn a_write_through_a_simple_view_reaches_the_base_table() {
    // The capability the catalog used to deny.
    let mut e = Engine::new();
    e.execute("CREATE TABLE ut (a int PRIMARY KEY, b text)").unwrap();
    e.execute("CREATE VIEW uv AS SELECT a, b FROM ut").unwrap();
    e.execute("INSERT INTO uv VALUES (1, 'x')").unwrap();
    e.execute("UPDATE uv SET b = 'y' WHERE a = 1").unwrap();
    assert_eq!(lines(&mut e, "SELECT a, b FROM ut"), vec!["1|y"]);
    e.execute("DELETE FROM uv WHERE a = 1").unwrap();
    assert_eq!(lines(&mut e, "SELECT count(*) FROM ut"), vec!["0"]);
}

#[test]
fn each_reason_reports_pgs_detail_and_hint() {
    let mut e = matrix_engine();
    // PG 18.4's exact DETAIL per reason, and the verb-specific HINT.
    assert_eq!(
        err(&mut e, "INSERT INTO na VALUES ('q', 1)"),
        "unsupported: cannot insert into view \"na\" DETAIL: Views containing GROUP BY are not \
         automatically updatable. HINT: To enable inserting into the view, provide an INSTEAD OF \
         INSERT trigger or an unconditional ON INSERT DO INSTEAD rule.",
    );
    assert_eq!(
        err(&mut e, "UPDATE nd SET a = 1"),
        "unsupported: cannot update view \"nd\" DETAIL: Views containing DISTINCT are not \
         automatically updatable. HINT: To enable updating the view, provide an INSTEAD OF UPDATE \
         trigger or an unconditional ON UPDATE DO INSTEAD rule.",
    );
    assert_eq!(
        err(&mut e, "DELETE FROM nl"),
        "unsupported: cannot delete from view \"nl\" DETAIL: Views containing LIMIT or OFFSET are \
         not automatically updatable. HINT: To enable deleting from the view, provide an INSTEAD \
         OF DELETE trigger or an unconditional ON DELETE DO INSTEAD rule.",
    );
    assert_eq!(
        err(&mut e, "DELETE FROM nj"),
        "unsupported: cannot delete from view \"nj\" DETAIL: Views that do not select from a \
         single table or view are not automatically updatable. HINT: To enable deleting from the \
         view, provide an INSTEAD OF DELETE trigger or an unconditional ON DELETE DO INSTEAD rule.",
    );
    assert_eq!(
        err(&mut e, "INSERT INTO nu VALUES (1)"),
        "unsupported: cannot insert into view \"nu\" DETAIL: Views containing UNION, INTERSECT, or \
         EXCEPT are not automatically updatable. HINT: To enable inserting into the view, provide \
         an INSTEAD OF INSERT trigger or an unconditional ON INSERT DO INSTEAD rule.",
    );
}

#[test]
fn a_with_clause_reports_its_own_reason() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ct (a int)").unwrap();
    e.execute("CREATE VIEW cv AS WITH w AS (SELECT a FROM ct) SELECT a FROM w")
        .unwrap();
    assert_eq!(
        err(&mut e, "INSERT INTO cv VALUES (1)"),
        "unsupported: cannot insert into view \"cv\" DETAIL: Views containing WITH are not \
         automatically updatable. HINT: To enable inserting into the view, provide an INSTEAD OF \
         INSERT trigger or an unconditional ON INSERT DO INSTEAD rule.",
    );
}

#[test]
fn when_several_reasons_apply_pg_names_exactly_one() {
    // Measured precedence on PG 18.4:
    //   set-op > DISTINCT > GROUP BY > WITH > LIMIT/OFFSET > not-single-table
    let mut e = Engine::new();
    e.execute("CREATE TABLE pt (a int, b text)").unwrap();
    e.execute("CREATE TABLE pt2 (x int)").unwrap();
    e.execute("CREATE VIEW m1 AS SELECT DISTINCT b FROM pt GROUP BY b")
        .unwrap();
    e.execute("CREATE VIEW m2 AS SELECT b FROM pt GROUP BY b UNION SELECT b FROM pt")
        .unwrap();
    e.execute("CREATE VIEW m3 AS SELECT pt.b FROM pt JOIN pt2 ON pt.a = pt2.x GROUP BY pt.b")
        .unwrap();
    e.execute("CREATE VIEW m4 AS WITH w AS (SELECT a FROM pt) SELECT a FROM w LIMIT 3")
        .unwrap();
    e.execute("CREATE VIEW m5 AS SELECT pt.a FROM pt JOIN pt2 ON pt.a = pt2.x LIMIT 3")
        .unwrap();
    let detail = |e: &mut Engine, sql: &str| {
        let m = err(e, sql);
        m.split(" DETAIL: ").nth(1).unwrap().split(" HINT")
            .next()
            .unwrap()
            .to_string()
    };
    // DISTINCT beats GROUP BY.
    assert_eq!(
        detail(&mut e, "INSERT INTO m1 VALUES ('q')"),
        "Views containing DISTINCT are not automatically updatable.",
    );
    // The set-op beats GROUP BY.
    assert_eq!(
        detail(&mut e, "INSERT INTO m2 VALUES ('q')"),
        "Views containing UNION, INTERSECT, or EXCEPT are not automatically updatable.",
    );
    // GROUP BY beats the join.
    assert_eq!(
        detail(&mut e, "INSERT INTO m3 VALUES ('q')"),
        "Views containing GROUP BY are not automatically updatable.",
    );
    // WITH beats LIMIT.
    assert_eq!(
        detail(&mut e, "INSERT INTO m4 VALUES (1)"),
        "Views containing WITH are not automatically updatable.",
    );
    // LIMIT beats the join.
    assert_eq!(
        detail(&mut e, "INSERT INTO m5 VALUES (1)"),
        "Views containing LIMIT or OFFSET are not automatically updatable.",
    );
}

#[test]
fn merge_names_merge_in_the_hint() {
    // PG reports the verb of the FIRST WHEN clause and offers only the
    // trigger, not a rewrite rule.
    let mut e = Engine::new();
    e.execute("CREATE TABLE gt (a int)").unwrap();
    e.execute("CREATE VIEW gv AS SELECT DISTINCT a FROM gt").unwrap();
    let m = err(
        &mut e,
        "MERGE INTO gv t USING (SELECT 1 AS k) s ON t.a = s.k \
         WHEN MATCHED THEN UPDATE SET a = s.k",
    );
    assert_eq!(
        m,
        "unsupported: cannot update view \"gv\" DETAIL: Views containing DISTINCT are not \
         automatically updatable. HINT: To enable updating the view using MERGE, provide an \
         INSTEAD OF UPDATE trigger.",
    );
}

#[test]
fn information_schema_tables_lists_views() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE vt (a int PRIMARY KEY, b text)").unwrap();
    e.execute("CREATE TABLE vt2 (x int, y text)").unwrap();
    e.execute("CREATE VIEW v_simple AS SELECT a, b FROM vt").unwrap();
    e.execute("CREATE VIEW v_agg AS SELECT b, count(*) AS c FROM vt GROUP BY b")
        .unwrap();
    e.execute("CREATE VIEW v_join AS SELECT vt.a, vt2.y FROM vt JOIN vt2 ON vt.a = vt2.x")
        .unwrap();
    e.execute("CREATE MATERIALIZED VIEW vm AS SELECT a FROM vt")
        .unwrap();
    // Live PG 18.4, verbatim — including that the materialized view is
    // absent. It is a real table underneath in SPG, and reporting it as
    // a BASE TABLE would have a migration tool recreate it as a table.
    assert_eq!(
        lines(
            &mut e,
            "SELECT table_name, table_type, is_insertable_into, is_typed \
             FROM information_schema.tables \
             WHERE table_name IN ('vt','vt2','v_simple','v_agg','v_join','vm') \
             ORDER BY table_name",
        ),
        vec![
            "v_agg|VIEW|NO|NO",
            "v_join|VIEW|NO|NO",
            "v_simple|VIEW|YES|NO",
            "vt|BASE TABLE|YES|NO",
            "vt2|BASE TABLE|YES|NO",
        ],
    );
}
