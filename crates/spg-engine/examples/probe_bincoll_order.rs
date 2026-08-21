//! v7.38.13 — is `ORDER BY` on a `COLLATE utf8mb4_bin` column byte-wise?
//! MySQL 9.7.1 answers `A, Bar, a, bar`. Controls so the answer can be
//! attributed rather than guessed: the dialect is asserted, not assumed,
//! and a column with NO declaration runs beside the one that has it.
use spg_engine::{Engine, QueryResult};

fn show(r: &QueryResult) -> String {
    match r {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|row| {
                row.values
                    .iter()
                    .map(|v| format!("{v:?}"))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect::<Vec<_>>()
            .join(" "),
        other => format!("{other:?}"),
    }
}

fn run(e: &mut Engine, sql: &str) {
    match e.execute(sql) {
        Ok(r) => println!("  {sql}\n      {}", show(&r)),
        Err(err) => println!("  {sql}\n      ERR {err:?}"),
    }
}

fn main() {
    for dialect in ["pg", "mysql"] {
        println!("\n======== dialect: {dialect} ========");
        let mut e = Engine::new();
        if dialect == "mysql" {
            // Assert, do not assume: a swallowed failure here would make
            // every row below a PG answer wearing a MySQL label.
            match e.execute("SET sql_mode = 'STRICT_TRANS_TABLES'") {
                Ok(_) => println!("  (sql_mode accepted)"),
                Err(err) => println!("  !! sql_mode REFUSED: {err:?}"),
            }
        }
        e.execute("CREATE TABLE binc (t VARCHAR(10) COLLATE utf8mb4_bin)")
            .expect("create binc");
        e.execute("CREATE TABLE plainc (t VARCHAR(10))")
            .expect("create plainc");
        for t in ["binc", "plainc"] {
            e.execute(&format!(
                "INSERT INTO {t} VALUES ('a'),('A'),('bar'),('Bar')"
            ))
            .expect("insert");
        }
        for t in ["binc", "plainc"] {
            println!("  --- {t} ---");
            run(&mut e, &format!("SELECT t FROM {t} ORDER BY t"));
            run(
                &mut e,
                &format!("SELECT t FROM {t} ORDER BY t COLLATE \"C\""),
            );
            run(
                &mut e,
                &format!("SELECT t FROM {t} ORDER BY t COLLATE \"en_US\""),
            );
            run(&mut e, &format!("SELECT MIN(t), MAX(t) FROM {t}"));
        }
    }
}
