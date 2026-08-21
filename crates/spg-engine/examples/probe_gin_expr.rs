//! PG's ordinary spelling for a full-text index puts an expression in
//! the key. SPG refused the DDL, so a schema using it did not load.
use spg_engine::{Engine, QueryResult};

fn n(e: &mut Engine, q: &str) -> String {
    match e.execute(q) {
        Ok(QueryResult::Rows { rows, .. }) => format!("{} row(s)", rows.len()),
        Ok(_) => "cmd".into(),
        Err(err) => format!(
            "ERR {}",
            format!("{err:?}").chars().take(70).collect::<String>()
        ),
    }
}

fn main() {
    let cases: [(&str, &str, &str); 4] = [
        (
            "concatenation",
            "CREATE INDEX g ON d USING gin (to_tsvector('english', title || ' ' || body))",
            "SELECT id FROM d WHERE to_tsvector('english', title || ' ' || body) @@ to_tsquery('english','fox')",
        ),
        (
            "coalesce",
            "CREATE INDEX g ON d USING gin (to_tsvector('english', coalesce(title,'')))",
            "SELECT id FROM d WHERE to_tsvector('english', coalesce(title,'')) @@ to_tsquery('english','quick')",
        ),
        (
            "jsonb path",
            "CREATE INDEX g ON d USING gin ((meta -> 'tags'))",
            "SELECT id FROM d WHERE meta -> 'tags' @> '[\"a\"]'",
        ),
        (
            "plain column (was already ok)",
            "CREATE INDEX g ON d USING gin (to_tsvector('english', body))",
            "SELECT id FROM d WHERE to_tsvector('english', body) @@ to_tsquery('english','lazy')",
        ),
    ];
    for (label, ddl, q) in cases {
        let mut e = Engine::new();
        e.execute("CREATE TABLE d (id INT, title TEXT, body TEXT, meta JSONB)")
            .unwrap();
        e.execute(
            "INSERT INTO d VALUES (1,'quick fox','jumps over the lazy dog','{\"tags\":[\"a\"]}')",
        )
        .unwrap();
        e.execute("INSERT INTO d VALUES (2,'slow turtle','sits','{\"tags\":[\"b\"]}')")
            .unwrap();
        let ddl_r = match e.execute(ddl) {
            Ok(_) => "built".to_string(),
            Err(err) => format!(
                "ERR {}",
                format!("{err:?}").chars().take(70).collect::<String>()
            ),
        };
        // A row inserted after the index was built must be findable too.
        let post = e
            .execute("INSERT INTO d VALUES (3,'quick hare','naps','{\"tags\":[\"a\"]}')")
            .map_or_else(|e| format!("{e:?}"), |_| "ok".into());
        println!(
            "{label:<30} {ddl_r:<12} insert {post:<4} query {}",
            n(&mut e, q)
        );
    }
}
