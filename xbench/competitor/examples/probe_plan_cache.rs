//! Verify-first: does a cached plan survive DDL that invalidates it?
use spg_engine::{Engine, QueryResult};

fn run(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => format!(
            "{:?}",
            rows.iter().map(|r| r.values.clone()).collect::<Vec<_>>()
        ),
        Ok(o) => format!("{o:?}"),
        Err(err) => format!("ERR({err})"),
    }
}

fn main() {
    // 1. ALTER ADD COLUMN then reuse the same SELECT text.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    println!("s1 pre : {}", run(&mut e, "SELECT * FROM t"));
    e.execute("ALTER TABLE t ADD COLUMN b INT DEFAULT 7")
        .unwrap();
    println!("s1 post: {}", run(&mut e, "SELECT * FROM t"));

    // 2. DROP + recreate with different schema, same SQL.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    println!("s2 pre : {}", run(&mut e, "SELECT * FROM t"));
    e.execute("DROP TABLE t").unwrap();
    e.execute("CREATE TABLE t (x TEXT, y INT)").unwrap();
    e.execute("INSERT INTO t VALUES ('a', 2)").unwrap();
    println!("s2 post: {}", run(&mut e, "SELECT * FROM t"));

    // 3. RENAME COLUMN then filter on the old plan's column.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT)").unwrap();
    e.execute("INSERT INTO t VALUES (5)").unwrap();
    println!("s3 pre : {}", run(&mut e, "SELECT a FROM t WHERE a = 5"));
    e.execute("ALTER TABLE t RENAME COLUMN a TO z").unwrap();
    println!(
        "s3 post (must ERR): {}",
        run(&mut e, "SELECT a FROM t WHERE a = 5")
    );

    // 4. RENAME TABLE.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT)").unwrap();
    e.execute("INSERT INTO t VALUES (9)").unwrap();
    println!("s4 pre : {}", run(&mut e, "SELECT * FROM t"));
    e.execute("ALTER TABLE t RENAME TO u").unwrap();
    println!("s4 post (must ERR): {}", run(&mut e, "SELECT * FROM t"));

    // 5. ALTER TYPE affecting a compiled WHERE.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT)").unwrap();
    e.execute("INSERT INTO t VALUES (5)").unwrap();
    println!(
        "s5 pre : {}",
        run(&mut e, "SELECT count(*) FROM t WHERE a > 3")
    );
    e.execute("ALTER TABLE t ALTER COLUMN a TYPE TEXT").unwrap();
    println!(
        "s5 post: {}",
        run(&mut e, "SELECT count(*) FROM t WHERE a > 3")
    );
}
