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
        // v7.39 (round 237) — NUMERIC prints its decimal text.
        // v7.39 (round 254) — via the engine's own renderer: this arm used
        // to ignore `kind`, so every NaN / ±Infinity numeric printed as its
        // canonical 0 and the sweep read 12 phantom diffs against PG.
        Value::Numeric { .. } => spg_engine::eval::value_to_text(v),
        // v7.39 (round 236) — arrays print PG's `{a,b}` external form.
        // Without this every array-returning probe row came back as
        // `IntArray([Some(1), Some(2)])` and buried the real diffs.
        Value::IntArray(items) => arr_text(items.iter().map(|v| v.map(|n| n.to_string()))),
        Value::BigIntArray(items) => arr_text(items.iter().map(|v| v.map(|n| n.to_string()))),
        Value::TextArray(items) => {
            arr_text(items.iter().map(|v| v.as_ref().map(ToString::to_string)))
        }
        // v7.39 (round 245) — everything else renders through the engine's
        // own wire-text formatter (tsvector/tsquery, ranges, geometry …),
        // which is what psql shows. The Debug fallback hid real diffs
        // behind struct noise for every non-scalar type.
        other => spg_engine::eval::value_to_text(other),
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
    // v7.39 (round 467) — `SPG_PROBE_MYSQL=1` runs the probe in the MySQL
    // dialect, the same switch mysqlwire flips per connection. Without it a
    // MySQL differential probes the PG path and proves nothing.
    if std::env::var("SPG_PROBE_MYSQL").is_ok_and(|v| v == "1") {
        e.set_mysql_wire_session();
    }
    // v7.39 (round 221) — named-timezone lookups, same wiring as
    // spg-embedded / spg-server, so tz differentials probe the real path.
    e.set_tz_fns(
        spg_tzif::tz_offset_at,
        spg_tzif::tz_local_to_utc,
        spg_tzif::tz_canonical,
        spg_tzif::tz_abbrev_at,
    );
    // v7.39 (round 247) — quote-aware statement splitting: a `;` inside a
    // string literal (`DELIMITER ';'`) is not a statement boundary. The
    // plain split shredded any statement carrying one.
    let mut stmts: Vec<String> = Vec::new();
    {
        let mut cur = String::new();
        let mut in_str = false;
        let mut chars = sql.chars().peekable();
        while let Some(c) = chars.next() {
            if in_str {
                cur.push(c);
                if c == '\'' {
                    if chars.peek() == Some(&'\'') {
                        cur.push(chars.next().unwrap());
                    } else {
                        in_str = false;
                    }
                }
            } else if c == '\'' {
                in_str = true;
                cur.push(c);
            } else if c == ';' {
                stmts.push(core::mem::take(&mut cur));
            } else {
                cur.push(c);
            }
        }
        stmts.push(cur);
    }
    for stmt in &stmts {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        // round 252 — `COPY … TO '<file>'`: the probe renders via the
        // engine and writes the file itself.
        if let Some(spec) = spg_engine::copy::parse_copy_to_file(stmt) {
            match e.copy_to_buffer(
                &spec.table,
                spec.columns.as_deref(),
                spec.query.as_deref(),
                &spec.options,
            ) {
                Ok((payload, _n)) => match std::fs::write(&spec.path, payload) {
                    Ok(()) => println!("(ok)"),
                    Err(err) => {
                        let os = err.to_string();
                        let os = os.split(" (os error").next().unwrap_or(&os).to_string();
                        println!(
                            "ERROR: could not open file \"{}\" for writing: {os}",
                            spec.path
                        );
                    }
                },
                Err(err) => println!("ERROR: {err}"),
            }
            continue;
        }
        // round 249 — `COPY … FROM '<file>'`: the probe is the host, so
        // it reads the file and feeds the engine's buffer endpoint.
        if let Some(spec) = spg_engine::copy::parse_copy_from_file(stmt) {
            match std::fs::read_to_string(&spec.path) {
                Ok(data) => match e.copy_from_buffer(
                    &spec.table,
                    spec.columns.as_deref(),
                    &spec.options,
                    &data,
                ) {
                    Ok(_) => println!("(ok)"),
                    Err(err) => println!("ERROR: {err}"),
                },
                Err(err) => {
                    let os = err.to_string();
                    let os = os.split(" (os error").next().unwrap_or(&os).to_string();
                    println!(
                        "ERROR: could not open file \"{}\" for reading: {os}",
                        spec.path
                    );
                }
            }
            continue;
        }
        let outcome = e.execute(stmt);
        // v7.39 (round 282) — surface the NOTICEs the statement raised.
        // The harness printed only the result, so an `IF EXISTS` skip
        // looked identical whether or not SPG raised PG's notice, and
        // the differential could not see the gap it was meant to find.
        for n in e.take_notices() {
            println!("{}: {}", n.severity.as_pg_str(), n.message);
        }
        match outcome {
            Ok(QueryResult::Rows { columns, rows }) => {
                // v7.39 (round 476) — render a TIMESTAMPTZ in the session's
                // zone, as the wire does.
                //
                // There is no scalar `Value::Timestamptz`: a timestamptz is a
                // `Value::Timestamp` and only the COLUMN says which it is. The
                // probe rendered the value alone, so `SET TIME ZONE
                // 'Asia/Tokyo'; SELECT now()` printed UTC with no offset while
                // a real client over pgwire correctly got `+09`. Every
                // timezone differential run through this probe was therefore
                // reporting a defect the server does not have — measured
                // against PG18 and against SPG's own wire, which agree.
                let tz = e.session_tz();
                let line: Vec<String> = rows
                    .iter()
                    .map(|row| {
                        row.values
                            .iter()
                            .enumerate()
                            .map(|(i, v)| match (v, columns.get(i).map(|c| c.ty)) {
                                (
                                    Value::Timestamp(micros),
                                    Some(spg_storage::DataType::Timestamptz),
                                ) => spg_engine::eval::format_timestamptz_tz(
                                    *micros,
                                    &e.render_style(),
                                    tz.offset_at(*micros),
                                    tz.abbrev_at(*micros).as_deref(),
                                ),
                                _ => render(v),
                            })
                            .collect::<Vec<_>>()
                            .join("|")
                    })
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
