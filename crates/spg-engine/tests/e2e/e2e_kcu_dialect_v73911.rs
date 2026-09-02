//! v7.39.11 — `information_schema.key_column_usage` serves the
//! session's own dialect's column list.
//!
//! Reported by sentori against 7.39.10, the mirror of the defect
//! 7.39.2 closed for `SHOW CREATE TABLE`: every session got MySQL's
//! shape, so a PostgreSQL session had no `table_schema` at all and
//! `WHERE table_schema = 'public'` — the way every tool scopes this
//! view — raised `column "table_schema" does not exist`, while
//! `referenced_table_name` and `referenced_column_name`, which
//! PostgreSQL does not have, were there instead.
//!
//! Measured on PostgreSQL 18.6 and MySQL 9.7.2. The two lists agree on
//! the first nine, in that order, and MySQL adds three:
//!
//! ```text
//!   PG 18.6    constraint_catalog, constraint_schema, constraint_name,
//!              table_catalog, table_schema, table_name, column_name,
//!              ordinal_position, position_in_unique_constraint
//!   MySQL 9.7  … the same nine, then referenced_table_schema,
//!              referenced_table_name, referenced_column_name
//! ```

use spg_engine::{Engine, QueryResult};

fn cols(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { columns, .. } =
        e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows");
    };
    columns.iter().map(|c| c.name.clone()).collect()
}

fn seeded(mysql: bool) -> Engine {
    let mut e = Engine::new();
    if mysql {
        e.set_mysql_dialect(true);
    }
    e.execute("CREATE TABLE parent (p int PRIMARY KEY)")
        .unwrap();
    e.execute("CREATE TABLE child (c int PRIMARY KEY, p int REFERENCES parent(p))")
        .unwrap();
    e
}

const PG_NINE: [&str; 9] = [
    "constraint_catalog",
    "constraint_schema",
    "constraint_name",
    "table_catalog",
    "table_schema",
    "table_name",
    "column_name",
    "ordinal_position",
    "position_in_unique_constraint",
];

#[test]
fn a_postgresql_session_gets_postgresqls_nine_columns_in_order() {
    let mut e = seeded(false);
    assert_eq!(
        cols(&mut e, "SELECT * FROM information_schema.key_column_usage"),
        PG_NINE
    );
}

#[test]
fn scoping_by_table_schema_is_how_every_tool_reads_this_view() {
    let mut e = seeded(false);
    let QueryResult::Rows { rows, .. } = e
        .execute(
            "SELECT constraint_name FROM information_schema.key_column_usage \
             WHERE table_schema = 'public' AND table_name = 'child' \
             ORDER BY constraint_name",
        )
        .expect("table_schema must exist on a PostgreSQL session")
    else {
        panic!("expected Rows")
    };
    assert_eq!(rows.len(), 2, "the primary key and the foreign key");
}

#[test]
fn a_mysql_session_still_gets_mysqls_three_extra_columns() {
    let mut e = seeded(true);
    let got = cols(&mut e, "SELECT * FROM information_schema.key_column_usage");
    assert_eq!(&got[..9], PG_NINE);
    assert_eq!(
        &got[9..],
        [
            "referenced_table_schema",
            "referenced_table_name",
            "referenced_column_name"
        ]
    );
}

#[test]
fn the_foreign_key_row_carries_what_it_references() {
    let mut e = seeded(true);
    let QueryResult::Rows { rows, .. } = e
        .execute(
            "SELECT referenced_table_name, referenced_column_name \
             FROM information_schema.key_column_usage \
             WHERE table_name = 'child' AND column_name = 'p'",
        )
        .unwrap()
    else {
        panic!("expected Rows")
    };
    let cells: Vec<String> = rows[0]
        .values
        .iter()
        .map(spg_engine::eval::value_to_text)
        .collect();
    assert_eq!(cells, ["parent", "p"]);
}
