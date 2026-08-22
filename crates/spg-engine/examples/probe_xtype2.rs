//! Mixed CHAR/VARCHAR text comparison across every shape a query can
//! take, under MySQL. Expectations are MySQL 9.7.2's own answers at its
//! default collation, read from the oracle.
use spg_engine::{Engine, QueryResult};
fn g(e: &mut Engine, s: &str) -> String {
    match e.execute(s) {
        Ok(QueryResult::Rows { rows, .. }) => {
            if rows.is_empty() {
                return "-".into();
            }
            rows.iter()
                .map(|r| {
                    r.values
                        .iter()
                        .map(|c| match c {
                            spg_storage::Value::Int(n) => n.to_string(),
                            spg_storage::Value::BigInt(n) => n.to_string(),
                            spg_storage::Value::Text(t) => t.trim_end().to_string(),
                            spg_storage::Value::BpChar(t) => t.trim_end().to_string(),
                            o => format!("{o:?}"),
                        })
                        .collect::<Vec<_>>()
                        .join("/")
                })
                .collect::<Vec<_>>()
                .join(",")
        }
        Ok(_) => "cmd".into(),
        Err(e) => format!(
            "ERR {}",
            format!("{e:?}").chars().take(34).collect::<String>()
        ),
    }
}
fn main() {
    let cases: [(&str, &str, &str); 9] = [
        (
            "x1 join char=varchar ",
            "SELECT a.k, b.k FROM a JOIN b ON a.c = b.s ORDER BY a.k, b.k",
            "1/10,2/20",
        ),
        (
            "x2 join varchar=char ",
            "SELECT a.k, b.k FROM a JOIN b ON a.s = b.c ORDER BY a.k, b.k",
            "1/10,2/20",
        ),
        (
            "x3 IN subquery       ",
            "SELECT k FROM a WHERE c IN (SELECT s FROM b) ORDER BY k",
            "1,2",
        ),
        (
            "x4 EXISTS            ",
            "SELECT k FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.s = a.c) ORDER BY k",
            "1,2",
        ),
        (
            "x5 UNION             ",
            "SELECT count(*) FROM (SELECT c AS v FROM a UNION SELECT s FROM b) u",
            "2",
        ),
        (
            "x6 nested scalar subq",
            "SELECT k FROM b WHERE b.s = (SELECT c FROM a WHERE k=1)",
            "10",
        ),
        (
            "x7 scalar subq rhs   ",
            "SELECT k FROM a WHERE c = (SELECT s FROM b WHERE k=10) ORDER BY k",
            "1",
        ),
        (
            "x8 DISTINCT over mix ",
            "SELECT count(DISTINCT v) FROM (SELECT c AS v FROM a UNION ALL SELECT s FROM b) t",
            "2",
        ),
        (
            "x9 CASE              ",
            "SELECT CASE a.c WHEN 'ALPHA' THEN 'hit' ELSE 'miss' END FROM a ORDER BY a.k",
            "hit,miss",
        ),
    ];
    let mut e = Engine::new();
    e.set_backslash_escapes(true);
    for s in [
        "CREATE TABLE a (k INT, c CHAR(8), s VARCHAR(32))",
        "CREATE TABLE b (k INT, c CHAR(8), s VARCHAR(32))",
        "INSERT INTO a VALUES (1,'alpha','alpha'),(2,'Beta','Beta')",
        "INSERT INTO b VALUES (10,'ALPHA','ALPHA'),(20,'beta','beta')",
    ] {
        e.execute(s).unwrap();
    }
    let mut bad = 0;
    for (label, q, want) in cases {
        let got = g(&mut e, q);
        let ok = got == want;
        if !ok {
            bad += 1;
        }
        println!(
            "{} mysql {want:<12} spg {} {got}",
            label,
            if ok { "ok " } else { "BAD" }
        );
    }
    println!("\n{bad}/9 disagree with MySQL 9.7.2");
}
