//! v7.39 (round 266) — information_schema constraint-reflection views.
//!
//! Every expectation below was read off live PG 18.4 with the same
//! fixture. The gaps this pins were all "the row/column a migration
//! tool selects is simply absent": NOT NULL pseudo-constraints existed
//! in `table_constraints` but in neither `check_constraints` nor
//! `constraint_column_usage`; CHECK constraints reached
//! `constraint_column_usage` not at all; and three views were missing
//! SQL-standard columns because they had been built from the
//! MySQL-flavoured column set.

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

/// Render one row the way `psql -tA` prints it, so the expectations can
/// be transcribed straight from the oracle.
fn line(row: &[spg_storage::Value<'static>]) -> String {
    row.iter()
        .map(|v| match v {
            spg_storage::Value::Null => String::new(),
            other => spg_engine::eval::value_to_text(other),
        })
        .collect::<Vec<_>>()
        .join("|")
}

fn lines(e: &mut Engine, sql: &str) -> Vec<String> {
    rows(e, sql).iter().map(|r| line(r)).collect()
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE zq_par (id int PRIMARY KEY, code text UNIQUE, n int CHECK (n > 0))")
        .unwrap();
    e.execute(
        "CREATE TABLE zq_child (cid int PRIMARY KEY, \
         pid int REFERENCES zq_par(id) ON DELETE CASCADE ON UPDATE SET NULL, \
         tag text NOT NULL)",
    )
    .unwrap();
    e
}

#[test]
fn referential_constraints_reports_the_sql_standard_columns() {
    let mut e = fixture();
    // PG 18.4: zq_child_pid_fkey|zq_par_pkey|NONE|SET NULL|CASCADE
    assert_eq!(
        lines(
            &mut e,
            "SELECT constraint_name, unique_constraint_name, match_option, \
             update_rule, delete_rule FROM information_schema.referential_constraints \
             ORDER BY constraint_name",
        ),
        vec!["zq_child_pid_fkey|zq_par_pkey|NONE|SET NULL|CASCADE"],
    );
    // The four qualifiers PG fills in; only the catalog name differs
    // from the oracle, because SPG's catalog is named `spg`.
    assert_eq!(
        lines(
            &mut e,
            "SELECT constraint_catalog, constraint_schema, unique_constraint_catalog, \
             unique_constraint_schema FROM information_schema.referential_constraints",
        ),
        vec!["spg|public|spg|public"],
    );
}

#[test]
fn match_full_is_reported_as_full() {
    // The default spells NONE, not SIMPLE; an explicit MATCH FULL — the
    // one SPG actually enforces differently — must be visible here.
    let mut e = Engine::new();
    e.execute("CREATE TABLE mf_par (id int PRIMARY KEY)")
        .unwrap();
    e.execute("CREATE TABLE mf_child (a int REFERENCES mf_par(id) MATCH FULL)")
        .unwrap();
    assert_eq!(
        lines(
            &mut e,
            "SELECT match_option FROM information_schema.referential_constraints",
        ),
        vec!["FULL"],
    );
}

#[test]
fn key_column_usage_carries_position_in_unique_constraint() {
    let mut e = fixture();
    // PG 18.4: FK rows carry the position inside the parent key; PK and
    // UNIQUE rows leave it NULL, which is how a reflection tool tells
    // the two kinds of row apart.
    assert_eq!(
        lines(
            &mut e,
            "SELECT constraint_name, position_in_unique_constraint \
             FROM information_schema.key_column_usage ORDER BY constraint_name",
        ),
        vec![
            "zq_child_pid_fkey|1",
            "zq_child_pkey|",
            "zq_par_code_key|",
            "zq_par_pkey|",
        ],
    );
}

#[test]
fn check_constraints_includes_the_not_null_rows() {
    let mut e = fixture();
    // PG 18.4 models NOT NULL as a real CHECK, so it appears here with
    // the clause it would have been written as. `table_constraints`
    // already listed these; this view did not, leaving the two views
    // disagreeing about the same constraint.
    assert_eq!(
        lines(
            &mut e,
            "SELECT constraint_name, check_clause FROM information_schema.check_constraints \
             ORDER BY constraint_name",
        ),
        vec![
            "zq_child_cid_not_null|cid IS NOT NULL",
            "zq_child_tag_not_null|tag IS NOT NULL",
            "zq_par_id_not_null|id IS NOT NULL",
            "zq_par_n_check|(n > 0)",
        ],
    );
}

#[test]
fn constraint_column_usage_covers_checks_and_not_nulls() {
    let mut e = fixture();
    // PG 18.4, verbatim. FK rows name the PARENT's columns; every other
    // kind names its own table's.
    assert_eq!(
        lines(
            &mut e,
            "SELECT constraint_name, table_name, column_name \
             FROM information_schema.constraint_column_usage \
             ORDER BY constraint_name, column_name",
        ),
        vec![
            "zq_child_cid_not_null|zq_child|cid",
            "zq_child_pid_fkey|zq_par|id",
            "zq_child_pkey|zq_child|cid",
            "zq_child_tag_not_null|zq_child|tag",
            "zq_par_code_key|zq_par|code",
            "zq_par_id_not_null|zq_par|id",
            "zq_par_n_check|zq_par|n",
            "zq_par_pkey|zq_par|id",
        ],
    );
}

#[test]
fn a_multi_column_check_explodes_across_its_columns() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE zw_t (a int NOT NULL, b int, c int, \
         CHECK (a + b > c), CONSTRAINT myck CHECK (c < 100))",
    )
    .unwrap();
    // PG 18.4: one row per column the expression mentions, and a
    // user-supplied constraint name wins over the synthesised one.
    assert_eq!(
        lines(
            &mut e,
            "SELECT constraint_name, table_name, column_name \
             FROM information_schema.constraint_column_usage \
             ORDER BY constraint_name, column_name",
        ),
        vec![
            "myck|zw_t|c",
            "zw_t_a_not_null|zw_t|a",
            "zw_t_check|zw_t|a",
            "zw_t_check|zw_t|b",
            "zw_t_check|zw_t|c",
        ],
    );
    assert_eq!(
        lines(
            &mut e,
            "SELECT constraint_name, check_clause FROM information_schema.check_constraints \
             ORDER BY constraint_name",
        ),
        vec![
            "myck|(c < 100)",
            "zw_t_a_not_null|a IS NOT NULL",
            "zw_t_check|((a + b) > c)",
        ],
    );
}

#[test]
fn tables_reports_the_standard_tail_columns() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE zt (a int)").unwrap();
    // PG 18.4 leaves every one of these NULL for an ordinary table and
    // answers YES/NO for the two flags. Selecting them used to raise
    // "column does not exist" instead of returning PG's NULL.
    assert_eq!(
        lines(
            &mut e,
            "SELECT table_name, table_type, is_insertable_into, is_typed, \
             self_referencing_column_name, reference_generation, commit_action \
             FROM information_schema.tables WHERE table_name = 'zt'",
        ),
        vec!["zt|BASE TABLE|YES|NO|||"],
    );
    assert_eq!(
        lines(
            &mut e,
            "SELECT user_defined_type_catalog, user_defined_type_schema, \
             user_defined_type_name FROM information_schema.tables WHERE table_name = 'zt'",
        ),
        vec!["||"],
    );
}
