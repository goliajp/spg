use spg_engine::{Engine, QueryResult};
fn n(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => format!("{}", rows.len()),
        Ok(o) => format!("{o:?}"),
        Err(x) => format!("ERR {x:?}"),
    }
}
fn main() {
    for (label, decl, want_equi, want_non) in [
        ("implicit CI ", "VARCHAR(20)", 2, 2),
        ("explicit bin", "VARCHAR(20) COLLATE utf8mb4_bin", 1, 1),
    ] {
        let mut e = Engine::new();
        e.execute("SET sql_mode = 'STRICT_TRANS_TABLES'").unwrap();
        e.execute(&format!("CREATE TABLE l (id INT, s {decl})"))
            .unwrap();
        e.execute(&format!("CREATE TABLE r (id INT, s {decl})"))
            .unwrap();
        e.execute("INSERT INTO l VALUES (1,'a'),(2,'b')").unwrap();
        e.execute("INSERT INTO r VALUES (1,'A'),(2,'b')").unwrap();
        let equi = n(&mut e, "SELECT l.id FROM l JOIN r ON l.s = r.s");
        let non = n(&mut e, "SELECT l.id FROM l JOIN r ON (l.s = r.s OR FALSE)");
        let ok = equi == want_equi.to_string() && non == want_non.to_string();
        println!(
            "{label}  equi={equi:<4} non-equi={non:<4}  MySQL wants {want_equi}/{want_non}  {}",
            if ok { "OK" } else { "!! MISMATCH" }
        );
    }
}
