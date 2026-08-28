//! PAD SPACE or NO PAD — SPG advertises `8.0.0-spg-v…` on the MySQL
//! wire, and MySQL 8.0's default collation `utf8mb4_0900_ai_ci` is NO
//! PAD. `mysql_compare_fold` trims trailing spaces unconditionally; its
//! own comment says "measured on MariaDB 11", whose default IS PAD
//! SPACE. So the rule was calibrated against the engine we do not claim
//! to be.
//!
//! Every expectation below was read from a live container today:
//! MySQL 9.7.2 with `utf8mb4_0900_ai_ci`, MariaDB 12.3.2 with
//! `utf8mb4_uca1400_ai_ci` — each engine's OWN default, declared
//! explicitly because the oracle containers pin `utf8mb4_bin` and a
//! measurement that forgets shows neither engine's real behaviour.
use spg_engine::{Engine, QueryResult};

fn cell(e: &mut Engine, q: &str) -> String {
    match e.execute(q) {
        Ok(QueryResult::Rows { rows, .. }) => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|c| match c {
                        spg_storage::Value::Int(n) => n.to_string(),
                        spg_storage::Value::BigInt(n) => n.to_string(),
                        spg_storage::Value::Text(s) => format!("[{s}]"),
                        spg_storage::Value::BpChar(s) => format!("[{}]", s.trim_end()),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .collect::<Vec<_>>()
            .join(","),
        Ok(_) => "cmd".into(),
        Err(err) => format!(
            "ERR {}",
            format!("{err:?}").chars().take(30).collect::<String>()
        ),
    }
}

fn setup(e: &mut Engine) {
    for sql in [
        "CREATE TABLE v (k INT, s TEXT)",
        "INSERT INTO v VALUES (1,'alpha'),(2,'alpha  '),(3,'Beta'),(4,'beta')",
        "CREATE TABLE c (k INT, s CHAR(8))",
        "INSERT INTO c VALUES (1,'alpha'),(2,'alpha  '),(3,'Beta'),(4,'beta')",
        "CREATE TABLE r (k INT, s TEXT)",
        "INSERT INTO r VALUES (10,'alpha'),(20,'ALPHA  ')",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
}

fn main() {
    // label, sql, mysql answer, mariadb answer
    let cases: [(&str, &str, &str, &str); 8] = [
        (
            "V-eq      ",
            "SELECT k FROM v WHERE s = 'alpha' ORDER BY k",
            "1",
            "1,2",
        ),
        (
            "V-in      ",
            "SELECT k FROM v WHERE s IN ('alpha','beta') ORDER BY k",
            "1,3,4",
            "1,2,3,4",
        ),
        ("V-distinct", "SELECT count(DISTINCT s) FROM v", "3", "2"),
        (
            "V-group   ",
            "SELECT count(*) FROM (SELECT s FROM v GROUP BY s) g",
            "3",
            "2",
        ),
        ("V-min     ", "SELECT min(s) FROM v", "[alpha]", "[alpha]"),
        (
            "V-join    ",
            "SELECT v.k, r.k FROM v JOIN r ON v.s = r.s ORDER BY v.k, r.k",
            "1/10,2/20",
            "1/10,1/20,2/10,2/20",
        ),
        (
            "C-eq      ",
            "SELECT k FROM c WHERE s = 'alpha' ORDER BY k",
            "1,2",
            "1,2",
        ),
        ("C-distinct", "SELECT count(DISTINCT s) FROM c", "2", "2"),
    ];
    let mut e = Engine::new();
    e.set_mysql_wire_session();
    setup(&mut e);
    let mut like_mysql = 0;
    let mut like_maria = 0;
    for (label, q, my, ma) in cases {
        let got = cell(&mut e, q);
        let m = if got == my {
            like_mysql += 1;
            "MySQL"
        } else {
            ""
        };
        let a = if got == ma {
            like_maria += 1;
            "MariaDB"
        } else {
            ""
        };
        let verdict = match (m.is_empty(), a.is_empty()) {
            (false, false) => "both agree".to_string(),
            (false, true) => "MySQL".to_string(),
            (true, false) => "MariaDB".to_string(),
            _ => "NEITHER".to_string(),
        };
        println!("{label}  mysql {my:<20} mariadb {ma:<20} spg {got:<20} -> {verdict}");
    }
    println!("\nagrees with MySQL {like_mysql}/8, with MariaDB {like_maria}/8");
}
