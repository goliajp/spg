use spg_engine::Engine;
fn q(e: &mut Engine, sql: &str) {
    match e.execute(sql) {
        Ok(spg_engine::QueryResult::Rows { rows, .. }) => {
            println!("-- {sql}");
            for r in rows.iter().take(12) { println!("   {:?}", r.values); }
            if rows.is_empty() { println!("   (0 rows)"); }
        }
        Ok(o) => println!("-- {sql} => {o:?}"),
        Err(x) => println!("-- {sql} => ERR {x}"),
    }
}
fn main() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("CREATE INDEX ix ON t (id)").unwrap();
    e.execute("CREATE SEQUENCE sq").unwrap();
    e.execute("CREATE SEQUENCE sq2").unwrap();
    e.execute("CREATE VIEW vv AS SELECT id FROM t").unwrap();
    e.execute("CREATE VIEW vw2 AS SELECT id FROM t").unwrap();
    for s in [
        "SELECT oid, relname, relkind FROM pg_class ORDER BY oid",
        "SELECT seqrelid FROM pg_sequence",
        "SELECT c.relname, s.seqstart FROM pg_class c JOIN pg_sequence s ON c.oid = s.seqrelid",
        "SELECT 'sq'::regclass, 'sq2'::regclass, 'vv'::regclass, 'vw2'::regclass",
        "SELECT c.oid, c.relname FROM pg_class c WHERE c.relkind='v'",
        "SELECT relname FROM pg_class WHERE oid = 'vv'::regclass",
        "SELECT relname FROM pg_class WHERE oid = 'sq'::regclass",
        "SELECT relname FROM pg_class WHERE oid = 't'::regclass",
        "SELECT relname FROM pg_class WHERE oid = 'ix'::regclass",
    ] { q(&mut e, s); }
}
