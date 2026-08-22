use spg_engine::{Engine, QueryResult};
fn g(e: &mut Engine, s: &str) -> String {
    match e.execute(s) {
        Ok(QueryResult::Rows { rows, .. }) => {
            if rows.is_empty() {
                "(empty)".into()
            } else {
                rows.iter()
                    .map(|r| format!("{:?}", r.values))
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        }
        Ok(_) => "cmd".into(),
        Err(e) => format!(
            "ERR {}",
            format!("{e:?}").chars().take(46).collect::<String>()
        ),
    }
}
fn main() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (k INT, c8 CHAR(8), v5 VARCHAR(5), n NUMERIC(10,3), b BIT(4), tv TSVECTOR, u UUID, by BYTEA, ts TIMESTAMP, d DATE, iv INTERVAL)").unwrap();
    e.execute("INSERT INTO t VALUES (1,'alpha','abc',12.345,B'1011',to_tsvector('a b'),'11111111-2222-3333-4444-555555555555','\\x0102','2026-01-02 03:04:05','2026-01-02', INTERVAL '1 day')").unwrap();
    for col in ["c8", "v5", "n", "b", "tv", "u", "by", "ts", "d", "iv"] {
        let direct = g(&mut e, &format!("SELECT {col} FROM t"));
        let subq = g(&mut e, &format!("SELECT (SELECT {col} FROM t WHERE k=1)"));
        let mark = if direct == subq { "ok " } else { "BAD" };
        println!("{mark} {col:<3} 直接 {direct:<46} 子查询 {subq}");
    }
}
