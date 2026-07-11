//! Real-gap probe: round/trunc/power/sign scale + extract seconds + to_char Month.
use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => {
            let v = &rows[0].values[0];
            println!("{sql:60} => {v:?}");
        }
        Ok(o) => println!("{sql:60} => {o:?}"),
        Err(err) => println!("{sql:60} => ERR({err})"),
    }
}

fn main() {
    let mut e = Engine::new();
    one(&mut e, "SELECT round(15, -1)");
    one(&mut e, "SELECT round(150, -2)");
    one(&mut e, "SELECT trunc(15, -1)");
    one(&mut e, "SELECT power(2, 8)");
    one(&mut e, "SELECT power(2.0, 8)");
    one(&mut e, "SELECT sign(1.5)");
    one(&mut e, "SELECT EXTRACT(SECOND FROM '2024-03-15 14:30:45.123456'::TIMESTAMP)");
    one(&mut e, "SELECT TO_CHAR('2024-03-15'::DATE, 'Month DD, YYYY')");
    one(&mut e, "SELECT pg_typeof(power(2, 8))");
}
