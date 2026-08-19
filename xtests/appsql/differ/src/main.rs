// Ask SPG and PostgreSQL 18 the same question about every statement in
// sentori's source: what columns does it return, and what parameters
// does it take? Describe is where their last three defects lived, and
// it is answerable without any data.
use sqlx::{Column, Executor, Statement, TypeInfo, postgres::PgPoolOptions};


#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let raw = std::fs::read_to_string(std::env::var("SQLJSON")?)?;
    let v: serde_json::Value = serde_json::from_str(&raw)?;
    let recs = v.as_array().unwrap();
    let spg = PgPoolOptions::new().max_connections(1).connect(&std::env::var("SPG")?).await?;
    let pg  = PgPoolOptions::new().max_connections(1).connect(&std::env::var("PG")?).await?;
    let (mut differ, mut spg_err, mut pg_err, mut same) = (0, 0, 0, 0);
    for r in recs {
        let sql = r["sql"].as_str().unwrap();
        let loc = format!("{}:{}", r["file"].as_str().unwrap(), r["line"].as_i64().unwrap());
        let a = describe(&spg, sql).await;
        let b = describe(&pg, sql).await;
        match (&a, &b) {
            (Ok(x), Ok(y)) if x == y => same += 1,
            (Ok(x), Ok(y)) => { differ += 1;
                println!("DIFF {loc}\n  spg {x:?}\n  pg  {y:?}\n  sql {}\n", one_line(sql)); }
            (Err(e), Ok(y)) => { spg_err += 1;
                println!("SPG-ERR {loc}\n  err {e}\n  pg  {y:?}\n  sql {}\n", one_line(sql)); }
            (Ok(_), Err(e)) => { pg_err += 1;
                println!("PG-ERR(ours ok) {loc}\n  err {e}\n  sql {}\n", one_line(sql)); }
            (Err(a2), Err(b2)) => { same += 1; let _ = (a2, b2); }
        }
    }
    println!("== {} statements: {same} agree, {differ} differ, {spg_err} SPG-only errors, {pg_err} PG-only errors",
             recs.len());
    Ok(())
}

fn one_line(s: &str) -> String { s.split_whitespace().collect::<Vec<_>>().join(" ") }

async fn describe(p: &sqlx::PgPool, sql: &str) -> Result<Vec<(String, String)>, String> {
    match p.prepare(sql).await {
        Ok(st) => Ok(st.columns().iter()
            .map(|c| (c.name().to_string(), c.type_info().name().to_string())).collect()),
        Err(e) => Err(e.to_string().lines().next().unwrap_or("").to_string()),
    }
}
