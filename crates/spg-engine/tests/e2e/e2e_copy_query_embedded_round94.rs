//! v7.39 (read01 round 94) — `COPY (<query>) TO STDOUT` through the embedded
//! SQL parser + engine (the non-wire path).
//!
//! The wire layer intercepts COPY, but the SQL parser must also accept the
//! query form so `engine.execute("COPY (SELECT …) TO STDOUT")` works — the
//! table form already did, and leaving the query form as a parse error would
//! split behaviour between the two entry points. exec renders one `copy` text
//! column, one row per COPY line (header first when asked), honouring the
//! same WITH options as the table form.

use spg_engine::{Engine, QueryResult};

fn copy_lines(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns.len(), 1, "COPY TO renders a single text column");
            assert_eq!(columns[0].name, "copy");
            rows.iter()
                .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
                .collect()
        }
        other => panic!("{sql}: {other:?}"),
    }
}

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE cq (a int, b text)").unwrap();
    e.execute("INSERT INTO cq VALUES (1,'x'),(2,'y'),(3,NULL)").unwrap();
    e
}

#[test]
fn embedded_query_text() {
    let mut e = seed();
    assert_eq!(
        copy_lines(&mut e, "COPY (SELECT a, b FROM cq WHERE a <> 2 ORDER BY a) TO STDOUT"),
        ["1\tx", "3\t\\N"]
    );
    assert_eq!(copy_lines(&mut e, "COPY (SELECT count(*) FROM cq) TO STDOUT"), ["3"]);
}

#[test]
fn embedded_query_csv_header() {
    let mut e = seed();
    assert_eq!(
        copy_lines(
            &mut e,
            "COPY (SELECT a, b FROM cq ORDER BY a) TO STDOUT WITH (FORMAT csv, HEADER)"
        ),
        ["a,b", "1,x", "2,y", "3,"]
    );
}

#[test]
fn embedded_query_round_trips_through_display() {
    // The parsed statement must Display back to the `COPY (<query>) TO STDOUT`
    // shape (used for WAL / catalog source rendering).
    use spg_sql::parser::parse_statement;
    let stmt = parse_statement("COPY (SELECT a FROM cq ORDER BY a) TO STDOUT").unwrap();
    let rendered = stmt.to_string();
    assert!(
        rendered.starts_with("COPY (SELECT") && rendered.contains(") TO STDOUT"),
        "unexpected Display: {rendered}"
    );
}
