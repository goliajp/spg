//! 9.0.0 — the last 27 catalog columns whose declared type was `text`,
//! and the three readers that fell out of declaring them.
//!
//! A sweep of all 365 shared `pg_*` columns against PostgreSQL 18.6
//! (`xtests/catalog-type-sweep/sweep.py`) ended on four types SPG had no
//! `DataType` for: `pg_node_tree` (12 columns, every stored expression a
//! catalog holds), `aclitem[]` (7), `anyarray` (6) and `"char"[]` (2).
//! Each cell keeps the text rendering PostgreSQL prints; only the
//! declaration was missing, and a driver that decodes by the announced
//! type was told `text`.
//!
//! Declaring them broke three things that had never been asked:
//!
//!   * `coalesce(relacl, 'NULL')` — how a query asks for an ACL without
//!     a NULL — resolves to the column's declared type and coerces the
//!     other branch into it. The coercion table did not know the type,
//!     so the query failed outright rather than answering.
//!   * Twelve `pg_attribute` rows named a type `pg_type` did not carry
//!     (194), which is exactly the defect round 640 closed for `xid`.
//!   * `oid` reached the type-name table's catch-all, so it read
//!     `USER-DEFINED` and `atttypid IN (194)` under a join was refused
//!     as `operator does not exist: USER-DEFINED = integer`. Measured on
//!     PostgreSQL 18.6 the answer is a row: `oid` compares against the
//!     integer widths (`1::oid = 1::int` is `t`) though NOT against
//!     `numeric` or `real` (`operator does not exist` there), which is
//!     why it is a pair rule and not a type category.

use spg_engine::{Engine, QueryResult};

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    rows.into_iter()
        .map(|r| spg_engine::eval::value_to_text(r.values.first().expect("one column")))
        .collect()
}

fn err(e: &mut Engine, sql: &str) -> String {
    alloc_fmt(e.execute(sql).unwrap_err())
}

fn alloc_fmt(err: spg_engine::EngineError) -> String {
    format!("{err}")
}

/// The 27 columns, named and typed as PostgreSQL 18.6 names and types
/// them — the sweep's own output, transcribed.
const DECLARED: &[(&str, &str)] = &[
    ("pg_attrdef.adbin", "pg_node_tree"),
    ("pg_attribute.attacl", "aclitem[]"),
    ("pg_attribute.attmissingval", "anyarray"),
    ("pg_class.relacl", "aclitem[]"),
    ("pg_class.relpartbound", "pg_node_tree"),
    ("pg_constraint.conbin", "pg_node_tree"),
    ("pg_index.indexprs", "pg_node_tree"),
    ("pg_index.indpred", "pg_node_tree"),
    ("pg_largeobject_metadata.lomacl", "aclitem[]"),
    ("pg_namespace.nspacl", "aclitem[]"),
    ("pg_policy.polqual", "pg_node_tree"),
    ("pg_policy.polwithcheck", "pg_node_tree"),
    ("pg_proc.proacl", "aclitem[]"),
    ("pg_proc.proargdefaults", "pg_node_tree"),
    ("pg_proc.proargmodes", "\"char\"[]"),
    ("pg_proc.prosqlbody", "pg_node_tree"),
    ("pg_statistic_ext.stxexprs", "pg_node_tree"),
    ("pg_statistic_ext.stxkind", "\"char\"[]"),
    ("pg_stats.histogram_bounds", "anyarray"),
    ("pg_stats.most_common_elems", "anyarray"),
    ("pg_stats.most_common_vals", "anyarray"),
    ("pg_stats.range_bounds_histogram", "anyarray"),
    ("pg_stats.range_length_histogram", "anyarray"),
    ("pg_tablespace.spcacl", "aclitem[]"),
    ("pg_trigger.tgqual", "pg_node_tree"),
    ("pg_type.typacl", "aclitem[]"),
    ("pg_type.typdefaultbin", "pg_node_tree"),
];

#[test]
fn the_last_twenty_seven_columns_announce_pg_s_type() {
    let mut e = Engine::new();
    let got = col(
        &mut e,
        "SELECT c.relname||'.'||a.attname||'='||format_type(a.atttypid,-1) \
         FROM pg_class c JOIN pg_attribute a ON a.attrelid=c.oid \
         WHERE a.atttypid IN (194,1002,1034,2277) ORDER BY 1",
    );
    let want: Vec<String> = DECLARED.iter().map(|(c, t)| format!("{c}={t}")).collect();
    assert_eq!(got, want);
}

#[test]
fn an_acl_column_coalesces_with_a_text_literal() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ac9 (id int)").unwrap();
    // The shape a query uses to read an ACL without a NULL. Before the
    // coercion table knew `aclitem[]` this failed with "storage: type
    // mismatch in column \"\" (position 0): expected ACLITEM[], got TEXT".
    assert_eq!(
        col(
            &mut e,
            "SELECT coalesce(relacl, 'NULL') FROM pg_class WHERE relname='ac9'"
        ),
        vec!["NULL".to_string()]
    );
    // And the same for a `pg_node_tree` column.
    assert_eq!(
        col(
            &mut e,
            "SELECT coalesce(relpartbound, '-') FROM pg_class WHERE relname='ac9'"
        ),
        vec!["-".to_string()]
    );
}

#[test]
fn every_declared_type_has_a_pg_type_row() {
    let mut e = Engine::new();
    assert_eq!(
        col(
            &mut e,
            "SELECT count(*) FROM pg_attribute a LEFT JOIN pg_type t ON t.oid = a.atttypid \
             WHERE t.oid IS NULL"
        ),
        vec!["0".to_string()]
    );
    assert_eq!(
        col(&mut e, "SELECT format_type(194,-1)"),
        vec!["pg_node_tree".to_string()]
    );
}

#[test]
fn an_oid_column_compares_against_an_integer_in_list() {
    let mut e = Engine::new();
    // The plan shape that resolves the column's declared type: a
    // restriction on the OTHER relation. `atttypid = 194` always
    // answered; `atttypid IN (194)` was refused.
    assert_eq!(
        col(
            &mut e,
            "SELECT count(*) FROM pg_class c JOIN pg_attribute a ON a.attrelid=c.oid \
             WHERE c.relname='pg_index' AND a.atttypid IN (194)"
        ),
        vec!["2".to_string()]
    );
}

#[test]
fn the_oid_family_is_a_pair_rule_not_a_category() {
    let mut e = Engine::new();
    // Measured on PostgreSQL 18.6: oid unifies with the three integer
    // widths and with the reg types, and with nothing else. It is not a
    // category because the relation is not transitive — int unifies with
    // numeric, oid does not.
    for sql in [
        "SELECT 1::oid UNION SELECT 1::int",
        "SELECT 1::oid UNION SELECT 1::smallint",
        "SELECT 1::oid UNION SELECT 1::bigint",
        "SELECT 1::oid UNION SELECT 1::regclass",
    ] {
        assert_eq!(col(&mut e, sql), vec!["1".to_string()], "{sql}");
    }
    // PG: "UNION types oid and text cannot be matched".
    assert!(
        err(&mut e, "SELECT 1::oid UNION SELECT 'x'::text").contains("cannot be matched"),
        "{}",
        err(&mut e, "SELECT 1::oid UNION SELECT 'x'::text")
    );
}

#[test]
fn array_is_information_schema_s_word_and_nobody_else_s() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE dt9 (a oid, b int[], c text)")
        .unwrap();
    // information_schema.columns says the single word ARRAY and leaves
    // the element to udt_name — and says `oid`, not `USER-DEFINED`.
    assert_eq!(
        col(
            &mut e,
            "SELECT column_name||'='||data_type FROM information_schema.columns \
             WHERE table_name='dt9' ORDER BY ordinal_position"
        ),
        vec![
            "a=oid".to_string(),
            "b=ARRAY".to_string(),
            "c=text".to_string()
        ]
    );
    // An error names the real type. PG 18.6: "operator does not exist:
    // integer[] + integer".
    assert!(
        err(&mut e, "SELECT ARRAY[1,2] + 1").contains("integer[] + integer"),
        "{}",
        err(&mut e, "SELECT ARRAY[1,2] + 1")
    );
    // And so does pg_prepared_statements, which also canonicalises the
    // spelling the user wrote: PG reads `{integer[],text}` for `int[]`.
    e.execute("PREPARE p9(int[], text) AS SELECT 1").unwrap();
    assert_eq!(
        col(
            &mut e,
            "SELECT parameter_types::text FROM pg_prepared_statements WHERE name='p9'"
        ),
        vec!["{integer[],text}".to_string()]
    );
}
