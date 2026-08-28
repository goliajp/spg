//! v7.39 — what a MySQL reflection reads off a column.
//!
//! `SELECT CHARACTER_SET_NAME FROM information_schema.COLUMNS` errored
//! with *column does not exist*, and `COLLATION_NAME` answered NULL for
//! every column that had no explicit `COLLATE`. An ORM reading those
//! back could not tell a case-insensitive column from a binary one.
//!
//! Expectations read off MySQL 9.7.2, which reports the pair for
//! character types and NULL for both on int, decimal, date, blob and
//! varbinary — and reports the EFFECTIVE collation, not only a declared
//! one. PostgreSQL's own view answers NULL there for a column at its
//! type's default, and keeps doing so on a PG session: the two dialects
//! disagree about the same column name, which is why the answer is
//! per-dialect rather than a second column of the same name.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected Rows from {sql}");
    };
    rows.iter()
        .map(|r| {
            r.values
                .iter()
                .map(|v| match v {
                    spg_storage::Value::Null => "<NULL>".to_string(),
                    other => spg_engine::eval::value_to_text(other),
                })
                .collect()
        })
        .collect()
}

const DDL: &str = "CREATE TABLE ct (a INT, b VARCHAR(5), d TEXT, e BYTEA, \
                   f DATE, h VARCHAR(5) COLLATE utf8mb4_bin, j NUMERIC(4,1))";

const Q: &str = "SELECT column_name, character_set_name, collation_name \
                 FROM information_schema.columns WHERE table_name = 'ct' \
                 ORDER BY column_name";

#[test]
fn a_mysql_session_reads_the_charset_and_collation_of_every_column() {
    let mut e = Engine::new();
    e.execute("SET sql_mode=''").unwrap();
    e.execute(DDL).unwrap();
    let got = rows(&mut e, Q);
    let want = [
        ("a", "<NULL>", "<NULL>"),
        ("b", "utf8mb4", "utf8mb4_0900_ai_ci"),
        ("d", "utf8mb4", "utf8mb4_0900_ai_ci"),
        ("e", "<NULL>", "<NULL>"),
        ("f", "<NULL>", "<NULL>"),
        // A declared collation MySQL knows is reported as declared.
        ("h", "utf8mb4", "utf8mb4_bin"),
        ("j", "<NULL>", "<NULL>"),
    ];
    assert_eq!(got.len(), want.len(), "{got:?}");
    for (row, (name, cs, coll)) in got.iter().zip(want) {
        assert_eq!(row[0], name, "{got:?}");
        assert_eq!(row[1], cs, "charset of {name}");
        assert_eq!(row[2], coll, "collation of {name}");
    }
}

#[test]
fn a_postgresql_session_keeps_pgs_view_of_the_same_columns() {
    let mut e = Engine::new();
    e.execute(DDL).unwrap();
    // No `character_set_name` at all on this side — PG has no such
    // column, and answering one would be a claim about a view PG owns.
    assert!(
        e.execute(Q)
            .unwrap_err()
            .to_string()
            .contains("character_set_name"),
        "PG's information_schema.columns has no character_set_name"
    );
    // And `collation_name` keeps PG's rule: the DECLARED name or NULL.
    let got = rows(
        &mut e,
        "SELECT column_name, collation_name FROM information_schema.columns \
         WHERE table_name = 'ct' ORDER BY column_name",
    );
    let coll: Vec<&str> = got.iter().map(|r| r[1].as_str()).collect();
    assert_eq!(
        coll,
        [
            "<NULL>",
            "<NULL>",
            "<NULL>",
            "<NULL>",
            "<NULL>",
            "utf8mb4_bin",
            "<NULL>"
        ],
        "PG reports what the DDL declared, and NULL where it declared nothing"
    );
}

#[test]
fn a_pg_spelling_in_a_collate_clause_is_not_echoed_as_a_mysql_collation() {
    // SPG accepts PG's names too. `C` is not a collation MySQL has ever
    // had, so the MySQL view reports the session default — a true
    // statement about how the column compares — rather than echoing it.
    let mut e = Engine::new();
    e.execute("SET sql_mode=''").unwrap();
    e.execute(r#"CREATE TABLE cp (s VARCHAR(5) COLLATE "C")"#)
        .unwrap();
    let got = rows(
        &mut e,
        "SELECT character_set_name, collation_name FROM information_schema.columns \
         WHERE table_name = 'cp'",
    );
    assert_eq!(got[0], ["utf8mb4", "utf8mb4_0900_ai_ci"]);
}

#[test]
fn the_schema_reports_its_default_charset_and_collation_to_mysql() {
    let mut e = Engine::new();
    e.execute("SET sql_mode=''").unwrap();
    // v7.39.2 — asked about the DATABASE, not about `public`.
    //
    // In MySQL a schema IS a database and this view lists databases;
    // `public` is a PostgreSQL namespace and MySQL has none, so it is no
    // longer a row here. Asking for it found nothing and this test
    // indexed row 0 of an empty answer — which is the right failure for
    // the wrong subject, not a lost capability: the charset and
    // collation are still reported, on the row a MySQL client reads.
    let got = rows(
        &mut e,
        "SELECT default_character_set_name, default_collation_name \
         FROM information_schema.schemata WHERE schema_name = 'spg'",
    );
    assert_eq!(got[0], ["utf8mb4", "utf8mb4_0900_ai_ci"]);
}

#[test]
fn the_schema_keeps_pgs_shape_on_a_pg_session() {
    let mut e = Engine::new();
    // PG 18.6 has `default_character_set_name` and leaves it NULL
    // (measured), and has no `default_collation_name` at all.
    let got = rows(
        &mut e,
        "SELECT default_character_set_name FROM information_schema.schemata \
         WHERE schema_name = 'public'",
    );
    assert_eq!(got[0], ["<NULL>"]);
    assert!(
        e.execute("SELECT default_collation_name FROM information_schema.schemata")
            .unwrap_err()
            .to_string()
            .contains("default_collation_name")
    );
}
