//! v7.38 (read01) — date_part() returns double precision while EXTRACT returns
//! numeric (PG 14+). So date_part('epoch', …) renders like PG's double
//! (1704067201, no trailing .000000) but extract(epoch …) keeps the numeric
//! form. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("rows"),
    }
}

#[test]
fn date_part_is_double() {
    let mut e = Engine::new();
    // date_part → double: epoch has no trailing zeros; a fractional second shows.
    assert_eq!(text(&mut e, "SELECT date_part('epoch', TIMESTAMP '2024-01-01 00:00:01')::text"), "1704067201");
    assert_eq!(text(&mut e, "SELECT date_part('second', TIMESTAMP '2024-01-01 00:00:01.5')::text"), "1.5");
    assert_eq!(text(&mut e, "SELECT date_part('dow', DATE '2024-01-07')::text"), "0");
    // EXTRACT stays numeric (keeps its trailing zeros).
    assert_eq!(text(&mut e, "SELECT extract(epoch FROM TIMESTAMP '2024-01-01 00:00:01')::text"), "1704067201.000000");
}
