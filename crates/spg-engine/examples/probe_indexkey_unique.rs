use spg_engine::{Engine, QueryResult};
fn run(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join(","),
        Ok(_) => "ok".into(),
        Err(err) => format!("ERR {err}"),
    }
}
fn main() {
    // UNIQUE / PRIMARY KEY on the two new key spaces.
    for (label, ddl, a, b) in [
        ("uniq_numeric", "v NUMERIC UNIQUE", "1.5", "1.50"),
        ("pk_numeric", "v NUMERIC PRIMARY KEY", "1.5", "1.500"),
        (
            "uniq_bytea",
            "v BYTEA UNIQUE",
            "'\\xdead'::bytea",
            "'\\xdead'::bytea",
        ),
        (
            "pk_bytea",
            "v BYTEA PRIMARY KEY",
            "'\\x00'::bytea",
            "'\\x00'::bytea",
        ),
        ("uniq_num_distinct", "v NUMERIC UNIQUE", "1.5", "1.6"),
    ] {
        let mut e = Engine::new();
        e.execute(&format!("CREATE TABLE t ({ddl})")).unwrap();
        let first = run(&mut e, &format!("INSERT INTO t VALUES ({a})"));
        let second = run(&mut e, &format!("INSERT INTO t VALUES ({b})"));
        println!("{label:<20} first={first:<6} second={second}");
    }

    // Persistence: build indexes, snapshot, reload, and ask again.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT PRIMARY KEY, n NUMERIC, b BYTEA)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 1.50, '\\xdead'::bytea), (2, -2, '\\x00'::bytea), (3, 'NaN', ''::bytea)").unwrap();
    e.execute("CREATE INDEX tn ON t (n)").unwrap();
    e.execute("CREATE INDEX tb ON t (b)").unwrap();
    let before = (
        run(&mut e, "SELECT id FROM t WHERE n = 1.5"),
        run(&mut e, "SELECT id FROM t WHERE b = '\\x00'::bytea"),
        run(&mut e, "SELECT id FROM t WHERE n > 0 ORDER BY id"),
    );
    let bytes = e.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).expect("reload");
    let mut e2 = Engine::restore(cat);
    let after = (
        run(&mut e2, "SELECT id FROM t WHERE n = 1.5"),
        run(&mut e2, "SELECT id FROM t WHERE b = '\\x00'::bytea"),
        run(&mut e2, "SELECT id FROM t WHERE n > 0 ORDER BY id"),
    );
    println!("snapshot bytes={}", bytes.len());
    println!("before={before:?}");
    println!(
        "after ={after:?}   {}",
        if before == after { "SAME" } else { "DIFF!" }
    );
}
