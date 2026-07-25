//! P0-9 — what does `stmt.ast.clone()` cost for a big VALUES statement?
//!
//! pgwire's `handle_execute` hands the engine a deep clone of the prepared
//! statement's AST on every Execute. For the panel's `insert_batch_1k` that
//! is 1000 tuples x 3 literals, and round 448 left ~2.5 ms of server-path
//! time unaccounted for on exactly that shape. Round 198 removed a deep
//! clone from the SQL-rendering path for this same statement; this one is
//! still there. Count it before assuming anything.

fn batch_sql(rows: usize) -> String {
    let mut s = String::with_capacity(rows * 24 + 32);
    s.push_str("INSERT INTO wb VALUES ");
    for k in 0..rows {
        if k > 0 {
            s.push(',');
        }
        s.push_str(&format!("({k},{},{})", k % 100, k * 7 % 100_000));
    }
    s
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    println!("# median of 51, µs");
    println!("| rows | parse | ast.clone() | to_string() |");
    println!("|------|------:|------------:|------------:|");
    for rows in [1usize, 100, 1000] {
        let sql = batch_sql(rows);
        let mut parse = Vec::new();
        let mut clone = Vec::new();
        let mut render = Vec::new();
        let ast = spg_sql::parser::parse_statement(&sql).expect("parse");
        for _ in 0..51 {
            let t = std::time::Instant::now();
            let a = spg_sql::parser::parse_statement(&sql).expect("parse");
            parse.push(t.elapsed().as_secs_f64() * 1e6);
            core::hint::black_box(&a);

            let t = std::time::Instant::now();
            let c = ast.clone();
            clone.push(t.elapsed().as_secs_f64() * 1e6);
            core::hint::black_box(&c);

            let t = std::time::Instant::now();
            let s = ast.to_string();
            render.push(t.elapsed().as_secs_f64() * 1e6);
            core::hint::black_box(&s);
        }
        println!(
            "| {rows:4} | {:5.1} | {:11.1} | {:11.1} |",
            median(parse),
            median(clone),
            median(render)
        );
    }
}
