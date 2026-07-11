//! VALUES / UNION common-type resolution probes vs the PG18 anchors:
//! (1),(2.0)→numeric · float8,(1.0)→float8 · (1),(2::bigint)→bigint ·
//! (NULL),(1.5)→numeric · count(DISTINCT mixed float/numeric)=4.

fn main() {
    use spg_engine::Engine;
    let mut eng = Engine::new();
    for sql in [
        "SELECT pg_typeof(x) FROM (VALUES (1),(2.0)) t(x) LIMIT 1",
        "SELECT pg_typeof(x) FROM (VALUES ('NaN'::float8),(1.0)) t(x) LIMIT 1",
        "SELECT pg_typeof(x) FROM (VALUES (1),(2::bigint)) t(x) LIMIT 1",
        "SELECT pg_typeof(x) FROM (VALUES (NULL),(1.5)) t(x) LIMIT 1",
        "SELECT count(DISTINCT x) FROM (VALUES ('NaN'::float8),(1.0),('NaN'),(2.0),('-0'::float8),(0.0)) t(x)",
        "SELECT x::text FROM (VALUES (1),(2.0)) t(x) ORDER BY x",
        "SELECT x FROM (VALUES ('NaN'::float8),(1.0)) t(x)",
        "VALUES ('NaN'::float8),(1.0)",
        "SELECT count(DISTINCT x) FROM (VALUES ('NaN'::float8),(1.0),('NaN')) t(x)",
        "SELECT DISTINCT x FROM (VALUES ('NaN'::float8),(1.0),('NaN')) t(x)",
        "SELECT count(DISTINCT x) FROM (VALUES ('-0'::float8),(0.0)) t(x)",
        "SELECT x FROM (VALUES ('-0'::float8),(0.0)) t(x)",
    ] {
        match eng.execute(sql) {
            Ok(spg_engine::QueryResult::Rows { rows, .. }) => {
                let cells: Vec<String> = rows
                    .iter()
                    .map(|r| format!("{:?}", r.values))
                    .collect();
                println!("{sql}\n  -> {}", cells.join(" | "));
            }
            Ok(other) => println!("{sql}\n  -> {other:?}"),
            Err(e) => println!("{sql}\n  -> ERR {e:?}"),
        }
    }
}
