use spg_engine::{Engine, QueryResult};
fn q(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        spg_storage::Value::Text(t) => t.to_string(),
                        o => format!("{o:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .collect::<Vec<_>>()
            .join(" "),
        Ok(o) => format!("{o:?}").chars().take(50).collect(),
        Err(x) => format!("ERR {x:?}").chars().take(120).collect(),
    }
}
fn main() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE tv (id INT, v TSVECTOR)").unwrap();
    // The round-trip must preserve the value, not merely avoid an error.
    println!(
        "INSERT..SELECT : {}",
        q(
            &mut e,
            "INSERT INTO tv SELECT g, to_tsvector('english', 'the quick fox ' || g::text) FROM generate_series(1,3) g"
        )
    );
    println!(
        "stored          : {}",
        q(&mut e, "SELECT id, v::text FROM tv ORDER BY id")
    );
    println!(
        "VALUES control  : {}",
        q(
            &mut e,
            "SELECT to_tsvector('english','the quick fox 1')::text"
        )
    );
    println!("the index still matches it:");
    println!(
        "  @@ query      : {}",
        q(
            &mut e,
            "SELECT COUNT(*) FROM tv WHERE v @@ to_tsquery('english','fox')"
        )
    );
    println!(
        "  negative      : {}",
        q(
            &mut e,
            "SELECT COUNT(*) FROM tv WHERE v @@ to_tsquery('english','elephant')"
        )
    );
}
