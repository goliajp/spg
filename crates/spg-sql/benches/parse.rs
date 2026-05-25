// Bench code allow-list — see crates/spg-crypto/benches/hash.rs for rationale.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::useless_conversion,
    clippy::similar_names
)]

//! Stone-level criterion bench for `spg-sql`. Measures the three SQL
//! shapes most common in the engine hot path: a tiny constant SELECT,
//! a single-table SELECT with WHERE+ORDER+LIMIT, and a multi-join
//! aggregate. Each bench measures lex + parse as one unit because the
//! engine never calls them separately.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use spg_sql::lexer;
use spg_sql::parser::parse_statement;

fn lex_only(sql: &str) {
    let _ = lexer::tokenize(sql).expect("lex ok");
}

fn parse_full(sql: &str) {
    let _ = parse_statement(sql).expect("parse ok");
}

fn bench_lex_short(c: &mut Criterion) {
    let sql = "SELECT 1";
    c.bench_function("lex_select_one", |b| {
        b.iter(|| lex_only(black_box(sql)));
    });
}

fn bench_parse_short(c: &mut Criterion) {
    let sql = "SELECT 1";
    c.bench_function("parse_select_one", |b| {
        b.iter(|| parse_full(black_box(sql)));
    });
}

fn bench_parse_typical(c: &mut Criterion) {
    let sql = "SELECT id, name FROM users WHERE id > 100 ORDER BY id DESC LIMIT 10";
    c.bench_function("parse_select_where_order_limit", |b| {
        b.iter(|| parse_full(black_box(sql)));
    });
}

fn bench_parse_join_aggregate(c: &mut Criterion) {
    let sql = "SELECT u.name, COUNT(*) FROM users AS u \
               INNER JOIN orders AS o ON u.id = o.user_id \
               WHERE o.total > 50 \
               GROUP BY u.name \
               HAVING COUNT(*) > 2 \
               ORDER BY 2 DESC LIMIT 20";
    c.bench_function("parse_join_aggregate", |b| {
        b.iter(|| parse_full(black_box(sql)));
    });
}

criterion_group!(
    benches,
    bench_lex_short,
    bench_parse_short,
    bench_parse_typical,
    bench_parse_join_aggregate
);
criterion_main!(benches);
