//! v7.38.14 Phase A lead — does a MySQL text equi-join answer differently
//! depending on which join stage the planner picks? The nested-loop path
//! folds case (resolve.rs:135); the hash stage builds keys with
//! `encode_key_refs_into`, which hard-codes mysql=false (aggregate.rs:7505).
//! If both are reachable for the same query, the ANSWER depends on the plan.
//!
//! Under the MySQL default collation 'A' = 'a', so every row must match.
use spg_engine::{Engine, QueryResult};

fn count(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => format!("{}", rows.len()),
        Ok(o) => format!("{o:?}"),
        Err(err) => format!("ERR {err:?}"),
    }
}

fn main() {
    for n in [5usize, 200, 5000] {
        let mut e = Engine::new();
        e.execute("SET sql_mode = 'STRICT_TRANS_TABLES'")
            .expect("dialect");
        e.execute("CREATE TABLE l (id INT, s VARCHAR(20))").unwrap();
        e.execute("CREATE TABLE r (id INT, s VARCHAR(20))").unwrap();
        // l holds lower case, r holds UPPER case, same letters.
        e.execute(&format!(
            "INSERT INTO l SELECT g, lower(chr(97 + (g % 20))) FROM generate_series(1,{n}) g"
        ))
        .unwrap();
        e.execute(&format!(
            "INSERT INTO r SELECT g, upper(chr(97 + (g % 20))) FROM generate_series(1,{n}) g"
        ))
        .unwrap();
        let joined = count(&mut e, "SELECT l.id FROM l JOIN r ON l.s = r.s");
        // Control: the same equality outside a join, which takes the
        // nested-loop/eval path for certain.
        let wherecmp = count(&mut e, "SELECT id FROM l WHERE s = 'A'");
        println!("rows={n:<5} join(l.s=r.s) -> {joined:<10} control WHERE s='A' -> {wherecmp}");
    }
}
