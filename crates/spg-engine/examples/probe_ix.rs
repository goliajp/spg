fn main() {
    for with_index in [false, true] {
        let mut e = spg_engine::Engine::new();
        e.execute("CREATE TABLE t(x text COLLATE \"en_US.utf8\")").unwrap();
        e.execute("INSERT INTO t VALUES ('Zebra'),('apple'),('DateStyle'),('client'),('Bob')").unwrap();
        if with_index { e.execute("CREATE INDEX ix ON t(x)").unwrap(); }
        for q in [
            "SELECT x FROM t ORDER BY x",
            "SELECT x FROM t WHERE x = 'apple'",
            "SELECT x FROM t WHERE x > 'b' ORDER BY x",
            "SELECT x FROM t WHERE x BETWEEN 'b' AND 'd' ORDER BY x",
        ] {
            let r = match e.execute(q) {
                Ok(spg_engine::QueryResult::Rows { rows, .. }) =>
                    rows.iter().map(|r| format!("{:?}", r.values[0])).collect::<Vec<_>>().join(" "),
                Ok(o) => format!("{o:?}"), Err(er) => format!("ERR {er:?}"),
            };
            println!("idx={:5} {:56} -> {}", with_index, q, r.replace("Text(", "").replace(")", "").replace("\"", ""));
        }
    }
}
