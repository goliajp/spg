//! v7.38.14 Phase A — is any index ever CHOSEN by the planner?
//! Accepting an index shape and never planning against it is silent:
//! right answers, no error, no acceleration.
use spg_engine::{Engine, QueryResult};

fn plan(e: &mut Engine, sql: &str) -> String {
    match e.execute(&format!("EXPLAIN {sql}")) {
        Ok(QueryResult::Rows { rows, .. }) => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        spg_storage::Value::Text(t) => t.to_string(),
                        o => format!("{o:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>()
            .join(" / "),
        Ok(o) => format!("{o:?}"),
        Err(err) => format!("ERR {err:?}"),
    }
}

fn main() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT PRIMARY KEY, k INT, s TEXT, j JSONB)")
        .unwrap();
    e.execute(
        "INSERT INTO t SELECT g, g, 'x' || g::text, ('{\"k\":' || g::text || '}')::jsonb \
         FROM generate_series(1,3000) g",
    )
    .unwrap();
    for ddl in [
        "CREATE INDEX idx_col  ON t (k)",
        "CREATE INDEX idx_expr ON t (lower(s))",
        "CREATE INDEX idx_gin  ON t USING gin (j)",
    ] {
        match e.execute(ddl) {
            Ok(_) => println!("ok   {ddl}"),
            Err(err) => println!("ERR  {ddl} -> {err:?}"),
        }
    }
    e.execute("ANALYZE t").ok();
    println!();
    for (label, sql) in [
        (
            "btree column   (CONTROL)",
            "SELECT count(*) FROM t WHERE k = 42",
        ),
        (
            "btree on expr           ",
            "SELECT count(*) FROM t WHERE lower(s) = 'x42'",
        ),
        (
            "gin containment         ",
            "SELECT count(*) FROM t WHERE j @> '{\"k\":42}'",
        ),
    ] {
        println!("{label}  {}", plan(&mut e, sql));
    }
}
