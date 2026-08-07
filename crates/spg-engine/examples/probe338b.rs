use spg_engine::Engine;
fn q(e: &mut Engine, sql: &str) {
    match e.execute(sql) {
        Ok(spg_engine::QueryResult::Rows { rows, .. }) => {
            println!("-- {sql}");
            for r in rows.iter().take(8) {
                println!("   {:?}", r.values);
            }
            if rows.is_empty() {
                println!("   (0 rows)");
            }
        }
        Ok(o) => println!("-- {sql} => {o:?}"),
        Err(x) => println!("-- {sql} => ERR {x}"),
    }
}
fn main() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT, v TEXT)").unwrap();
    e.execute("CREATE VIEW vv AS SELECT id FROM t").unwrap();
    e.execute("CREATE MATERIALIZED VIEW mv AS SELECT id FROM t")
        .unwrap();
    for s in [
        "SELECT relname, relkind FROM pg_class ORDER BY relname",
        "SELECT attname FROM pg_attribute WHERE attrelid = 'vv'::regclass",
        "SELECT count(*) FROM pg_attribute WHERE attrelid = 't'::regclass AND attnum > 0",
        "SELECT relname FROM pg_class WHERE relkind = 'm'",
        "SELECT 'mv'::regclass",
        "SELECT table_name, table_type FROM information_schema.tables ORDER BY table_name",
    ] {
        q(&mut e, s);
    }
}
