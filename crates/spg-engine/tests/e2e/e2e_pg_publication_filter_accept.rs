//! v7.37.21 (21.2 + 21.3) — publication row filter + column list
//! parse-accept-discard for pg_dump round-trip compatibility.

use spg_engine::Engine;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn publication_with_column_list_parses() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE orders (id INT, customer INT, total NUMERIC)");
    ddl(
        &mut e,
        "CREATE PUBLICATION pub_a FOR TABLE orders (id, customer)",
    );
}

#[test]
fn publication_with_row_filter_parses() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE orders (id INT, status TEXT)");
    ddl(
        &mut e,
        "CREATE PUBLICATION pub_b FOR TABLE orders WHERE (status = 'paid')",
    );
}

#[test]
fn publication_with_col_list_and_row_filter_parses() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE orders (id INT, customer INT, status TEXT)");
    ddl(
        &mut e,
        "CREATE PUBLICATION pub_c FOR TABLE orders (id, customer) WHERE (status = 'paid')",
    );
}

#[test]
fn publication_with_multiple_tables_each_with_modifiers() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE a (id INT, name TEXT)");
    ddl(&mut e, "CREATE TABLE b (id INT, amount NUMERIC)");
    ddl(
        &mut e,
        "CREATE PUBLICATION pub_d FOR TABLE a (id) WHERE (id > 0), b (id, amount)",
    );
}

#[test]
fn publication_filter_table_visible_in_pg_publication() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE orders (id INT, status TEXT)");
    ddl(
        &mut e,
        "CREATE PUBLICATION pub_e FOR TABLE orders (id) WHERE (status = 'paid')",
    );
    // pg_publication should carry the publication regardless of
    // whether the filter is enforced.
    let r = e
        .execute(
            "SELECT pubname FROM pg_catalog.pg_publication WHERE pubname = 'pub_e'",
        )
        .unwrap();
    let spg_engine::QueryResult::Rows { rows, .. } = r else {
        panic!("Rows");
    };
    assert_eq!(rows.len(), 1, "got {rows:?}");
}
