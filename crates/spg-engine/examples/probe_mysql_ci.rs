//! MySQL folds text comparisons; SPG's B-trees are keyed by bytes. Every
//! shape below is one where an index could answer, so every one is a
//! place the index can change the ANSWER.
//!
//! Each line runs the same query twice on the same data — once with no
//! index, once with one. Under MySQL the two must agree, and must agree
//! with MySQL 9.7 (default collation `utf8mb4_0900_ai_ci`).
use spg_engine::{Engine, QueryResult};

fn alloc_num(n: i64) -> String {
    n.to_string()
}

fn rows(e: &mut Engine, q: &str) -> String {
    match e.execute(q) {
        Ok(QueryResult::Rows { rows, .. }) => {
            // NOT sorted: two of these queries are about ORDER, and a
            // tidy-looking sort here is how the first run of this probe
            // reported them as agreeing when they did not.
            let v: Vec<String> = rows
                .iter()
                .map(|r| {
                    r.values
                        .iter()
                        .map(|c| match c {
                            spg_storage::Value::Int(n) => alloc_num(*n as i64),
                            spg_storage::Value::BigInt(n) => alloc_num(*n),
                            spg_storage::Value::Text(t) => t.to_string(),
                            spg_storage::Value::BpChar(t) => t.trim_end().to_string(),
                            other => format!("{other:?}"),
                        })
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .collect();
            v.join(",")
        }
        Ok(_) => "cmd".into(),
        Err(e) => format!(
            "ERR {}",
            format!("{e:?}").chars().take(40).collect::<String>()
        ),
    }
}

fn setup(e: &mut Engine, ty: &str, index: Option<&str>) {
    e.execute(&format!("CREATE TABLE t (k INT, s {ty})"))
        .unwrap();
    e.execute("INSERT INTO t VALUES (1,'alpha'),(2,'Beta'),(3,'GAMMA'),(4,'delta')")
        .unwrap();
    if let Some(d) = index {
        e.execute(d).unwrap();
    }
}

fn main() {
    // Third column is MySQL 9.7.1's own answer, taken from the oracle
    // (default collation utf8mb4_0900_ai_ci), not from reasoning.
    let queries = [
        ("equality      ", "SELECT k FROM t WHERE s = 'ALPHA'", "1"),
        (
            "IN list       ",
            "SELECT k FROM t WHERE s IN ('ALPHA','BETA')",
            "1,2",
        ),
        (
            "range >=      ",
            "SELECT k FROM t WHERE s >= 'DELTA' ORDER BY k",
            "3,4",
        ),
        (
            "BETWEEN       ",
            "SELECT k FROM t WHERE s BETWEEN 'ALPHA' AND 'DELTA' ORDER BY k",
            "1,2,4",
        ),
        ("ORDER BY s    ", "SELECT k FROM t ORDER BY s", "1,2,4,3"),
        (
            "ORDER BY LIMIT",
            "SELECT k FROM t ORDER BY s LIMIT 2",
            "1,2",
        ),
        ("MIN           ", "SELECT MIN(s) FROM t", "alpha"),
        ("count DISTINCT", "SELECT count(DISTINCT s) FROM t", "4"),
    ];
    for ty in ["TEXT", "VARCHAR(32)", "CHAR(8)"] {
        println!("=== {ty} ===");
        for (label, q, mysql_says) in queries {
            let mut a = Engine::new();
            a.set_backslash_escapes(true);
            setup(&mut a, ty, None);
            let bare = rows(&mut a, q);

            let mut b = Engine::new();
            b.set_backslash_escapes(true);
            setup(&mut b, ty, Some("CREATE INDEX t_s ON t (s)"));
            let idx = rows(&mut b, q);

            let mark = |got: &str| if got == mysql_says { "ok " } else { "BAD" };
            println!(
                "{label}  mysql {mysql_says:<9} | no-index {} {bare:<12} | indexed {} {idx}",
                mark(&bare),
                mark(&idx)
            );
        }
        // A unique index has its own answer to give.
        let mut c = Engine::new();
        c.set_backslash_escapes(true);
        c.execute(&format!("CREATE TABLE u (k INT, s {ty})"))
            .unwrap();
        c.execute("CREATE UNIQUE INDEX u_s ON u (s)").unwrap();
        c.execute("INSERT INTO u VALUES (1,'alpha')").unwrap();
        let dup = match c.execute("INSERT INTO u VALUES (2,'ALPHA')") {
            Ok(_) => "ACCEPTED",
            Err(_) => "rejected",
        };
        println!("   UNIQUE dup    'ALPHA' after 'alpha' -> {dup}  (MySQL rejects)");
        println!();
    }
}
