//! r1049 — sentori report 2, the non-blocking half.
//!
//! Two more positions where the `[]` array suffix did not parse —
//! the function PARAMETER list and the PREPARE type list — plus the
//! describe-side defect their suite could not even reach: DML with
//! RETURNING described as NoData, which made sqlx size the row at
//! zero columns and fail with ColumnIndexOutOfBounds.
//!
//! PG18.4, measured live: `CREATE FUNCTION sf(v bigint[])` and
//! `PREPARE pp(bigint[]) AS SELECT $1` both succeed, and
//! `EXECUTE pp('{1,2}')` answers `{1,2}`.

use spg_engine::{Engine, QueryResult};

/// The fifth and sixth members of the `[]` family. The column,
/// ALTER-ADD, cast and (r1038) RETURNS positions already parsed.
#[test]
fn r1049_array_suffix_parses_in_parameter_positions() {
    let mut e = Engine::new();
    e.execute("CREATE FUNCTION sf(v bigint[]) RETURNS int AS $$ SELECT 1 $$ LANGUAGE sql")
        .expect("CREATE FUNCTION with an array parameter must parse");
    e.execute("PREPARE pp(bigint[]) AS SELECT $1")
        .expect("PREPARE with an array parameter type must parse");
    match e.execute("EXECUTE pp('{1,2}')").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(spg_engine::eval::value_to_text(&rows[0].values[0]), "{1,2}");
        }
        other => panic!("EXECUTE pp: {other:?}"),
    }
    // `[N]` and multi-word type names keep working beside the suffix.
    e.execute("CREATE FUNCTION sf2(a int[3], b double precision) RETURNS void AS $$ SELECT 1 $$ LANGUAGE sql")
        .expect("array length and multi-word types in the parameter list");
}

/// DML + RETURNING describes its result set. The zero-column answer
/// was invisible to every text-format client (the row stream carries
/// its own RowDescription) and fatal to sqlx, which trusts Describe.
#[test]
fn r1049_dml_returning_describes_its_columns() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE dr (id BIGINT NOT NULL, name TEXT)")
        .unwrap();
    let cases = [
        ("INSERT INTO dr VALUES (1, 'a') RETURNING id", vec!["id"]),
        (
            "UPDATE dr SET name = 'b' RETURNING id, name",
            vec!["id", "name"],
        ),
        ("DELETE FROM dr RETURNING name", vec!["name"]),
        // No RETURNING: still NoData, as before.
        ("INSERT INTO dr VALUES (2, 'c')", vec![]),
    ];
    for (sql, want) in cases {
        let stmt =
            spg_sql::parser::parse_statement(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
        let (_, cols) = e.describe_prepared(&stmt);
        let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, want, "{sql}");
    }
    // And the described types are the table's, not a guess: id is BIGINT.
    let stmt =
        spg_sql::parser::parse_statement("INSERT INTO dr VALUES (3, 'd') RETURNING id, name")
            .unwrap();
    let (_, cols) = e.describe_prepared(&stmt);
    assert_eq!(cols[0].ty, spg_storage::DataType::BigInt, "{cols:?}");
    assert_eq!(cols[1].ty, spg_storage::DataType::Text, "{cols:?}");
}
