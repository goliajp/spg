#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args,
    clippy::unusual_byte_groupings
)]

//! v4.31 SQL parser fuzz — randomized inputs against
//! `parser::parse_statement`. The parser must never panic; an
//! adversarial input may legitimately error, just not crash the
//! process.
//!
//! By default runs `DEFAULT_ITERS` inputs (~10K). Set
//! `SPG_FUZZ_ITERS=N` for longer runs.

use std::time::Instant;

const DEFAULT_ITERS: u64 = 10_000;

struct SplitMix64 {
    state: u64,
}
impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn next_byte(&mut self) -> u8 {
        (self.next_u64() & 0xFF) as u8
    }
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + ((self.next_u64() as usize) % (hi - lo))
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.range(0, xs.len())]
    }
}

fn iters() -> u64 {
    std::env::var("SPG_FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_ITERS)
}

const KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "INSERT",
    "INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE",
    "CREATE",
    "TABLE",
    "INDEX",
    "DROP",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "AS",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "TRUE",
    "FALSE",
    "GROUP",
    "BY",
    "HAVING",
    "ORDER",
    "LIMIT",
    "OFFSET",
    "UNION",
    "ALL",
    "DISTINCT",
    "INNER",
    "JOIN",
    "LEFT",
    "ON",
    "WITH",
    "RECURSIVE",
    "WINDOW",
    "OVER",
    "PARTITION",
    "RANGE",
    "ROWS",
    "BETWEEN",
    "PRECEDING",
    "FOLLOWING",
    "CURRENT",
    "ROW",
    "UNBOUNDED",
    "EXISTS",
    "IN",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "IS",
    "LIKE",
    "CAST",
    "EXTRACT",
    "INTERVAL",
    "DATE",
    "TIMESTAMP",
    "INT",
    "BIGINT",
    "TEXT",
    "FLOAT",
    "BOOL",
    "VECTOR",
    "JSON",
    "NUMERIC",
    "DEFAULT",
];

const PUNCT: &[char] = &[
    ',', ';', '(', ')', '+', '-', '*', '/', '<', '>', '=', '!', '.', '\'', '"', '[', ']', '{', '}',
    ' ', '\n', '\t',
];

fn synthesize_sql(rng: &mut SplitMix64) -> String {
    let target_len = rng.range(0, 200);
    let mut out = String::with_capacity(target_len);
    while out.len() < target_len {
        match rng.next_byte() & 0b11 {
            0 => {
                out.push_str(rng.pick(KEYWORDS));
                out.push(' ');
            }
            1 => {
                let n = rng.range(1, 8);
                for _ in 0..n {
                    let c = (b'a' + (rng.next_byte() % 26)) as char;
                    out.push(c);
                }
                out.push(' ');
            }
            2 => {
                let v = rng.next_u64() % 100_000;
                out.push_str(&v.to_string());
                out.push(' ');
            }
            _ => out.push(rng.pick(PUNCT)),
        }
    }
    out
}

#[test]
fn fuzz_parse_statement_does_not_panic() {
    use spg_sql::parser;
    let mut rng = SplitMix64::new(0xC0FFEE_BABE);
    let n = iters();
    let started = Instant::now();
    let mut ok = 0u64;
    let mut err = 0u64;
    for _ in 0..n {
        let sql = synthesize_sql(&mut rng);
        match parser::parse_statement(&sql) {
            Ok(_) => ok += 1,
            Err(_) => err += 1,
        }
    }
    eprintln!(
        "sql fuzz: {n} iters in {:?}, parse-ok={ok} parse-err={err}",
        started.elapsed()
    );
    assert_eq!(ok + err, n);
}

/// Targeted at edge cases the random synth would miss: empty,
/// pathological UTF-8, very deep nesting, very long literals.
#[test]
fn parse_handles_pathological_inputs_without_panic() {
    use spg_sql::parser;
    for sql in [
        "",
        ";",
        " \t\n",
        "SELECT",
        "(((((((((((((((((((((((((((((((((1)))))))))))))))))))))))))))))))))",
        "SELECT '\\0\\x00\\x01\\x02\\xff' FROM t",
        "INSERT INTO t VALUES (",
        &"SELECT ".to_string().repeat(1000),
        &"a,".repeat(5000),
        // Multi-byte UTF-8 — must be rejected cleanly, not crash.
        "SELECT 中文 FROM 表",
        "SELECT * FROM t WHERE x = $$$$$$$$",
    ] {
        let _ = parser::parse_statement(sql);
    }
}
