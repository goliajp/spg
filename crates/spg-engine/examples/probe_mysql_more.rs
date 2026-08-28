//! The equality/IN/range/ordering seeks were the sites found by hand.
//! There are five more places a key is built from a column and a value
//! (`join.rs`, `select.rs`, two in `table_access.rs`, `subquery.rs`).
//! This asks the same question of the shapes those serve, rather than
//! reading them: same data, same query, index versus no index.
use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, q: &str) -> String {
    match e.execute(q) {
        Ok(QueryResult::Rows { rows, .. }) => {
            let v: Vec<String> = rows
                .iter()
                .map(|r| {
                    r.values
                        .iter()
                        .map(|c| match c {
                            spg_storage::Value::Int(n) => n.to_string(),
                            spg_storage::Value::BigInt(n) => n.to_string(),
                            spg_storage::Value::Text(t) => t.to_string(),
                            other => format!("{other:?}"),
                        })
                        .collect::<Vec<_>>()
                        .join("/")
                })
                .collect();
            v.join(",")
        }
        Ok(_) => "cmd".into(),
        Err(e) => format!(
            "ERR {}",
            format!("{e:?}").chars().take(36).collect::<String>()
        ),
    }
}

fn setup(e: &mut Engine, index: bool) {
    e.execute("CREATE TABLE a (k INT, s TEXT)").unwrap();
    e.execute("CREATE TABLE b (k INT, s TEXT)").unwrap();
    e.execute("INSERT INTO a VALUES (1,'alpha'),(2,'Beta')")
        .unwrap();
    e.execute("INSERT INTO b VALUES (10,'ALPHA'),(20,'beta')")
        .unwrap();
    if index {
        e.execute("CREATE INDEX a_s ON a (s)").unwrap();
        e.execute("CREATE INDEX b_s ON b (s)").unwrap();
    }
}

fn main() {
    // Third column is MySQL 9.7.1's answer, from the oracle.
    let queries: [(&str, &str, &str); 9] = [
        (
            "inner join    ",
            "SELECT a.k, b.k FROM a JOIN b ON a.s = b.s ORDER BY a.k",
            "1/10,2/20",
        ),
        (
            "left join     ",
            "SELECT a.k, b.k FROM a LEFT JOIN b ON a.s = b.s ORDER BY a.k",
            "1/10,2/20",
        ),
        (
            "IN subquery   ",
            "SELECT k FROM a WHERE s IN (SELECT s FROM b) ORDER BY k",
            "1,2",
        ),
        (
            "EXISTS        ",
            "SELECT k FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.s = a.s) ORDER BY k",
            "1,2",
        ),
        (
            "scalar subq   ",
            "SELECT (SELECT k FROM b WHERE b.s = 'ALPHA')",
            "10",
        ),
        (
            "count where   ",
            "SELECT count(*) FROM a WHERE s = 'ALPHA'",
            "1",
        ),
        // Pushed-down predicate on a joined table (table_access.rs).
        (
            "join + where  ",
            "SELECT a.k FROM a JOIN b ON a.k * 10 = b.k WHERE b.s = 'ALPHA' ORDER BY a.k",
            "1",
        ),
        // count(*) answered from index keys alone, no row re-check
        // (select.rs try_count_star_pk_in_list_fast).
        (
            "count IN list ",
            "SELECT count(*) FROM a WHERE s IN ('ALPHA','BETA')",
            "2",
        ),
        (
            "count range   ",
            "SELECT count(*) FROM a WHERE s >= 'ALPHA'",
            "2",
        ),
    ];
    for (label, q, mysql_says) in queries {
        let mut n = Engine::new();
        n.set_mysql_wire_session();
        setup(&mut n, false);
        let bare = rows(&mut n, q);
        let mut i = Engine::new();
        i.set_mysql_wire_session();
        setup(&mut i, true);
        let idx = rows(&mut i, q);
        let mark = |g: &str| if g == mysql_says { "ok " } else { "BAD" };
        println!(
            "{label} mysql {mysql_says:<10} | no-index {} {bare:<10} | indexed {} {idx}",
            mark(&bare),
            mark(&idx)
        );
    }
}
