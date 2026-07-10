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
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::SmallInt(n) => n.to_string(),
        // psql renders booleans as t / f.
        Value::Bool(b) => (if *b { "t" } else { "f" }).to_string(),
        // Non-scalar / float / numeric values keep the Debug form; wrap the
        // column in ::text in the probe SQL when an exact value diff is needed.
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
                // One line per statement: columns joined by `|`, rows by `;`
                // (matches `psql -tA` with rows collapsed onto a line).
                let line: Vec<String> = rows
                    .iter()
                    .map(|row| row.values.iter().map(render).collect::<Vec<_>>().join("|"))
                    .collect();
                println!("{}", line.join(";"));
            }
            Ok(_) => println!("(ok)"),
            Err(err) => println!("ERROR: {err}"),
        }
    }
}
