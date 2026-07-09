//! Ad-hoc differential probe: run each `;`-terminated statement from a file
//! (arg 1) against a fresh Engine and print result rows as `psql -tA` would
//! (values `|`-joined, NULL empty), or `ERROR: …`. Intended to be diffed
//! against `docker exec … psql -tA`. Not shipped; a dev aid only.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Text(s) => s.to_string(),
        other => format!("{other:?}"),
    }
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: probe <sql-file>");
    let sql = std::fs::read_to_string(&path).expect("read sql file");
    let mut e = Engine::new();
    for stmt in sql.split(';') {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        match e.execute(stmt) {
            Ok(QueryResult::Rows { rows, .. }) => {
                for row in &rows {
                    let cells: Vec<String> = row.values.iter().map(render).collect();
                    println!("{}", cells.join("|"));
                }
            }
            Ok(_) => println!("(ok)"),
            Err(err) => println!("ERROR: {err}"),
        }
    }
}
