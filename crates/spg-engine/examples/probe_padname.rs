//! The pad attribute is the collation NAME's. Measured on MySQL 9.7.2
//! and MariaDB 12.3.2, which agree per name:
//!   utf8mb4_0900_ai_ci  NO PAD     COUNT(DISTINCT) 3   WHERE = 1
//!   utf8mb4_bin         PAD SPACE                  3               2
//!   utf8mb4_general_ci  PAD SPACE                  2               2
use spg_engine::{Engine, QueryResult};
fn n(e: &mut Engine, q: &str) -> String {
    match e.execute(q).unwrap() {
        QueryResult::Rows { rows, .. } => format!("{:?}", rows[0].values[0]),
        _ => "?".into(),
    }
}
fn main() {
    let mut bad = 0;
    for (name, wd, we) in [
        ("utf8mb4_0900_ai_ci", "3", "1"),
        ("utf8mb4_bin", "3", "2"),
        ("utf8mb4_general_ci", "2", "2"),
        ("utf8mb4_unicode_ci", "2", "2"),
        ("utf8mb4_0900_bin", "4", "1"),
    ] {
        let mut e = Engine::new();
        e.set_mysql_wire_session();
        e.execute(&format!("CREATE TABLE t (s VARCHAR(32) COLLATE {name})"))
            .unwrap();
        e.execute("INSERT INTO t VALUES ('alpha'),('alpha  '),('Beta'),('beta')")
            .unwrap();
        let d = n(&mut e, "SELECT count(DISTINCT s) FROM t");
        let q = n(&mut e, "SELECT count(*) FROM t WHERE s = 'alpha'");
        let mut m = |g: &str, w: &str| {
            if g.contains(w) {
                "ok "
            } else {
                bad += 1;
                "BAD"
            }
        };
        let md = m(&d, wd);
        let mq = m(&q, we);
        println!("{name:<22} distinct {md} {d:<11} (want {wd})   eq {mq} {q:<11} (want {we})");
    }
    println!("\n{bad} 格与竞品不符");
}
