//! C12 — MySQL's diagnostics area. Every code and wording is from a
//! MySQL 9.7.2 run at `sql_mode=''`.
use spg_engine::{Engine, QueryResult};
fn g(e: &mut Engine, s: &str) -> String {
    match e.execute(s) {
        Ok(QueryResult::Rows { rows, .. }) => {
            if rows.is_empty() {
                "(no rows)".into()
            } else {
                rows.iter()
                    .map(|r| {
                        r.values
                            .iter()
                            .map(|c| match c {
                                spg_storage::Value::Text(t) => t.to_string(),
                                spg_storage::Value::Int(n) => n.to_string(),
                                spg_storage::Value::BigInt(n) => n.to_string(),
                                o => format!("{o:?}"),
                            })
                            .collect::<Vec<_>>()
                            .join("|")
                    })
                    .collect::<Vec<_>>()
                    .join("  ")
            }
        }
        Ok(_) => "cmd ok".into(),
        Err(e) => format!(
            "ERR {}",
            format!("{e:?}").chars().take(50).collect::<String>()
        ),
    }
}
fn main() {
    // label, sql, what MySQL 9.7.2 says
    let steps: [(&str, &str, &str); 8] = [
        (
            "truncate text ",
            "INSERT INTO w VALUES (1,'toolong')",
            "cmd ok",
        ),
        ("  count       ", "SELECT @@warning_count", "1"),
        (
            "  show        ",
            "SHOW WARNINGS",
            "Warning|1265|Data truncated for column 's' at row 1",
        ),
        (
            "bad integer   ",
            "INSERT INTO w VALUES ('abc','ok')",
            "cmd ok",
        ),
        (
            "  show        ",
            "SHOW WARNINGS",
            "Warning|1366|Incorrect integer value: 'abc' for column 'i' at row 1",
        ),
        (
            "out of range  ",
            "INSERT INTO w VALUES (99999999999,'ok')",
            "cmd ok",
        ),
        (
            "  show        ",
            "SHOW WARNINGS",
            "Warning|1264|Out of range value for column 'i' at row 1",
        ),
        ("clean insert  ", "INSERT INTO w VALUES (5,'ok')", "cmd ok"),
    ];
    let mut e = Engine::new();
    e.set_mysql_wire_session();
    e.execute("SET sql_mode = ''").unwrap();
    e.execute("CREATE TABLE w (i INT, s VARCHAR(3))").unwrap();
    let mut bad = 0;
    for (label, sql, want) in steps {
        let got = g(&mut e, sql);
        let ok = got == want;
        if !ok {
            bad += 1;
        }
        println!(
            "{label} {} {}",
            if ok { "ok " } else { "BAD" },
            if ok {
                got
            } else {
                format!("{got}   (want {want})")
            }
        );
    }
    // MySQL clears the area on the next warning-generating statement.
    let after = g(&mut e, "SELECT @@warning_count");
    let ok = after == "0";
    if !ok {
        bad += 1;
    }
    println!(
        "clean 之后计数  {} {after}   (MySQL 说 0)",
        if ok { "ok " } else { "BAD" }
    );
    println!("\n{bad} 格与 MySQL 9.7.2 不符");
}
