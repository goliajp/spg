//! v7.39 (round 268) — a view's columns in information_schema.columns.
//!
//! The view reported NO rows at all for a view before this round, so a
//! reflection tool saw every view as a relation with no columns. The
//! resolution runs over the view's stored body and reuses the same
//! row builder tables go through, so the two cannot describe the same
//! column shape differently.
//!
//! Every expectation was read off live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn lines(e: &mut Engine, sql: &str) -> Vec<String> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows from {sql}");
    };
    rows.into_iter()
        .map(|row| {
            row.values
                .iter()
                .map(|v| match v {
                    spg_storage::Value::Null => String::new(),
                    other => spg_engine::eval::value_to_text(other),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

#[test]
fn a_views_columns_are_listed_with_their_types() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE vt (a int NOT NULL, b text, c numeric(8,2))")
        .unwrap();
    e.execute("CREATE VIEW v1 AS SELECT a, b, c FROM vt").unwrap();
    // PG 18.4: is_nullable is YES even for `a`, which is NOT NULL on the
    // base table — a view's rows are a query result and PG does not
    // carry the base constraint through.
    assert_eq!(
        lines(
            &mut e,
            "SELECT column_name, ordinal_position, data_type, is_nullable \
             FROM information_schema.columns WHERE table_name = 'v1' \
             ORDER BY ordinal_position",
        ),
        vec!["a|1|integer|YES", "b|2|text|YES", "c|3|numeric|YES"],
    );
    // Precision travels with the column.
    assert_eq!(
        lines(
            &mut e,
            "SELECT numeric_precision, numeric_scale FROM information_schema.columns \
             WHERE table_name = 'v1' AND column_name = 'c'",
        ),
        vec!["8|2"],
    );
    // The per-column attributes PG reports for a plain updatable view.
    assert_eq!(
        lines(
            &mut e,
            "SELECT column_name, is_updatable, column_default, is_identity, is_generated \
             FROM information_schema.columns WHERE table_name = 'v1' ORDER BY ordinal_position",
        ),
        vec!["a|YES||NO|NEVER", "b|YES||NO|NEVER", "c|YES||NO|NEVER"],
    );
}

#[test]
fn a_join_view_resolves_across_both_sides() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE jt (a int)").unwrap();
    e.execute("CREATE TABLE jt2 (x int, y varchar(30))").unwrap();
    e.execute("CREATE VIEW jv AS SELECT jt.a, jt2.y FROM jt JOIN jt2 ON jt.a = jt2.x")
        .unwrap();
    // PG 18.4: a|integer||32|0 and y|character varying|30||
    assert_eq!(
        lines(
            &mut e,
            "SELECT column_name, data_type, character_maximum_length, numeric_precision, \
             numeric_scale FROM information_schema.columns WHERE table_name = 'jv' \
             ORDER BY ordinal_position",
        ),
        vec!["a|integer||32|0", "y|character varying|30||"],
    );
}

#[test]
fn computed_columns_are_not_updatable_but_are_described() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ct (a int, b text)").unwrap();
    e.execute("CREATE VIEW cv AS SELECT a AS ren, a + 1 AS calc, upper(b) AS ub FROM ct")
        .unwrap();
    // PG 18.4: the view stays updatable; only the expression columns
    // report is_updatable = NO.
    assert_eq!(
        lines(
            &mut e,
            "SELECT column_name, data_type, is_updatable FROM information_schema.columns \
             WHERE table_name = 'cv' ORDER BY ordinal_position",
        ),
        vec!["ren|integer|YES", "calc|integer|NO", "ub|text|NO"],
    );
}

#[test]
fn window_columns_report_their_result_types() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE wt (a int, b text, c numeric(8,2))")
        .unwrap();
    e.execute(
        "CREATE VIEW wv AS SELECT row_number() OVER () AS rn, rank() OVER (ORDER BY a) AS rk, \
         dense_rank() OVER (ORDER BY a) AS dr, ntile(4) OVER (ORDER BY a) AS nt, \
         percent_rank() OVER (ORDER BY a) AS pr, cume_dist() OVER (ORDER BY a) AS cd, \
         lag(b) OVER (ORDER BY a) AS lg, lead(c) OVER (ORDER BY a) AS ld, \
         first_value(a) OVER () AS fv, nth_value(b, 2) OVER () AS nv, \
         count(*) OVER () AS ct, sum(a) OVER () AS sm, avg(a) OVER () AS av, \
         max(c) OVER () AS mx FROM wt",
    )
    .unwrap();
    // Live PG 18.4, verbatim. Before this round a single window column
    // collapsed the whole view to zero described columns.
    assert_eq!(
        lines(
            &mut e,
            "SELECT column_name, data_type FROM information_schema.columns \
             WHERE table_name = 'wv' ORDER BY ordinal_position",
        ),
        vec![
            "rn|bigint",
            "rk|bigint",
            "dr|bigint",
            "nt|integer",
            "pr|double precision",
            "cd|double precision",
            "lg|text",
            "ld|numeric",
            "fv|integer",
            "nv|text",
            "ct|bigint",
            "sm|bigint",
            "av|numeric",
            "mx|numeric",
        ],
    );
}

#[test]
fn sum_and_avg_promote_the_way_the_runtime_already_did() {
    // These were described as the argument's own type while the engine
    // returned a promoted value — a RowDescription that disagreed with
    // the bytes that follow it. Types measured on PG 18.4.
    let mut e = Engine::new();
    e.execute("CREATE TABLE at (i2 smallint, i4 int, i8 bigint, n numeric(8,2), f float8)")
        .unwrap();
    e.execute(
        "CREATE VIEW av AS SELECT sum(i2) AS s2, sum(i4) AS s4, sum(i8) AS s8, sum(n) AS sn, \
         sum(f) AS sf, avg(i4) AS a4, avg(i8) AS a8, avg(n) AS an, avg(f) AS af, \
         min(i4) AS mi, max(i4) AS ma FROM at",
    )
    .unwrap();
    assert_eq!(
        lines(
            &mut e,
            "SELECT column_name, data_type FROM information_schema.columns \
             WHERE table_name = 'av' ORDER BY ordinal_position",
        ),
        vec![
            "s2|bigint",
            "s4|bigint",
            "s8|numeric",
            "sn|numeric",
            "sf|double precision",
            "a4|numeric",
            "a8|numeric",
            "an|numeric",
            "af|double precision",
            "mi|integer",
            "ma|integer",
        ],
    );
    // The runtime always did this correctly; the pin guards that the
    // description now agrees with it.
    e.execute("INSERT INTO at (i4) VALUES (2000000000), (2000000000), (2000000000)")
        .unwrap();
    assert_eq!(lines(&mut e, "SELECT sum(i4) FROM at"), vec!["6000000000"]);
}

#[test]
fn a_view_over_a_view_resolves_through_the_chain() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE nt (a int, b text)").unwrap();
    e.execute("CREATE VIEW n1 AS SELECT a, b FROM nt").unwrap();
    e.execute("CREATE VIEW n2 AS SELECT a FROM n1").unwrap();
    assert_eq!(
        lines(
            &mut e,
            "SELECT column_name, data_type, is_updatable FROM information_schema.columns \
             WHERE table_name = 'n2' ORDER BY ordinal_position",
        ),
        vec!["a|integer|YES"],
    );
}

#[test]
fn a_non_updatable_views_columns_are_all_read_only() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE gt (a int, b text)").unwrap();
    e.execute("CREATE VIEW gv AS SELECT b, count(*) AS c FROM gt GROUP BY b")
        .unwrap();
    assert_eq!(
        lines(
            &mut e,
            "SELECT column_name, data_type, is_updatable FROM information_schema.columns \
             WHERE table_name = 'gv' ORDER BY ordinal_position",
        ),
        vec!["b|text|NO", "c|bigint|NO"],
    );
}
