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
        // v7.39 (round 234) — JSON / JSONB print their text form, as psql
        // does. Without this every json-returning probe row came back as
        // `Json("{\"a\": 1}")` and drowned real diffs in escaping noise.
        Value::Json(s) => s.to_string(),
        // v7.39 (round 236) — arrays print PG's `{a,b}` external form.
        // Without this every array-returning probe row came back as
        // `IntArray([Some(1), Some(2)])` and buried the real diffs.
        Value::IntArray(items) => arr_text(items.iter().map(|v| v.map(|n| n.to_string()))),
        Value::BigIntArray(items) => arr_text(items.iter().map(|v| v.map(|n| n.to_string()))),
        Value::TextArray(items) => {
            arr_text(items.iter().map(|v| v.as_ref().map(ToString::to_string)))
        }
        // Non-scalar / float / numeric values keep the Debug form; wrap the
        // column in ::text in the probe SQL when an exact value diff is needed.
        other => format!("{other:?}"),
    }
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: probe <sql-file>");
    let sql = std::fs::read_to_string(&path).expect("read sql file");
    fn wall_clock_micros() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0)
    }
    let mut e = Engine::new().with_clock(wall_clock_micros);
    // v7.39 (round 221) — named-timezone lookups, same wiring as
    // spg-embedded / spg-server, so tz differentials probe the real path.
    e.set_tz_fns(
        spg_tzif::tz_offset_at,
        spg_tzif::tz_local_to_utc,
        spg_tzif::tz_canonical,
        spg_tzif::tz_abbrev_at,
    );
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

/// PG's array external form: `{a,b,NULL}`, quoting members that need it.
fn arr_text(items: impl Iterator<Item = Option<String>>) -> String {
    let body: Vec<String> = items
        .map(|v| match v {
            None => "NULL".to_string(),
            Some(s) => {
                if s.is_empty() || s.contains([',', '{', '}', '"', ' ']) {
                    format!("\"{}\"", s.replace('"', "\\\""))
                } else {
                    s
                }
            }
        })
        .collect();
    format!("{{{}}}", body.join(","))
}
