//! spg-gendiff — the generative differ (7.38 S4.2, design D15).
//!
//! Structured generation, three-legged execution, automatic shrink:
//!
//! 1. **Generate.** A seeded LCG mutates a parsed skeleton AST
//!    (`SELECT … FROM t1 [JOIN t2 …]`) — projections, predicates,
//!    ORDER BY, GROUP BY/HAVING, DISTINCT, LIMIT are all built as
//!    `spg_sql::ast` values and printed by the crate's own `Display`.
//!    No text splicing: every candidate derives from a well-formed
//!    tree. (D15 asked for from-scratch AST construction; skeletons
//!    come from one `parse_statement` call because the leaf structs
//!    deliberately don't implement `Default` — the mutation surface
//!    is still purely structural.)
//! 2. **Differ.** Each statement runs on the embedded engine and over
//!    BOTH wire protocols (simple + extended) against a REAL
//!    spg-server seeded with the same schema. All three answers must
//!    agree after normalisation; agreeing errors count as agreement.
//! 3. **Shrink.** A divergence is minimised by clause-dropping (the
//!    statement-level half of ddmin — one statement has no statement
//!    list to bisect) and the survivor is drafted to
//!    `xtests/sqllogictest/corpus/15_regressions/gen_<seed>_<n>.test`
//!    for human review before it becomes a pinned fixture.
//!
//! The seed prints on every run and replays the whole session:
//! `spg-gendiff --seed N --count M`.

use spg_sql::ast::{
    BinOp, ColumnName, Expr, Literal, OrderBy, SelectItem, SelectStatement, Statement,
};
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

/// Deterministic LCG (Numerical Recipes constants) — the whole run is
/// a pure function of the seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 17
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

const SETUP: &[&str] = &[
    "CREATE TABLE t1 (a INT NOT NULL, b INT, c TEXT)",
    "CREATE TABLE t2 (x INT NOT NULL, y TEXT)",
    "INSERT INTO t1 SELECT g, CASE WHEN g % 7 = 0 THEN NULL ELSE g * 3 END, \
     CASE WHEN g % 5 = 0 THEN NULL ELSE 'r' || (g % 13) END FROM generate_series(1, 200) g",
    "INSERT INTO t2 SELECT g, 's' || (g % 9) FROM generate_series(1, 80) g",
];

/// Columns the generator may reference, with their host table.
const COLS: &[(&str, &str)] = &[
    ("t1", "a"),
    ("t1", "b"),
    ("t1", "c"),
    ("t2", "x"),
    ("t2", "y"),
];
const NUM_COLS: &[(&str, &str)] = &[("t1", "a"), ("t1", "b"), ("t2", "x")];

fn col(q: &str, n: &str) -> Expr {
    Expr::Column(ColumnName {
        qualifier: Some(q.to_string()),
        name: n.to_string(),
    })
}

fn int(v: i64) -> Expr {
    Expr::Literal(Literal::Integer(v))
}

fn gen_num_expr(rng: &mut Rng, joined: bool, depth: u8) -> Expr {
    let pool: &[(&str, &str)] = if joined { NUM_COLS } else { &NUM_COLS[..2] };
    let leaf = |rng: &mut Rng| {
        if rng.chance(30) {
            int(i64::try_from(rng.below(40)).expect("small") - 5)
        } else {
            let (q, n) = pool[usize::try_from(rng.below(pool.len() as u64)).expect("idx")];
            col(q, n)
        }
    };
    if depth == 0 || rng.chance(50) {
        return leaf(rng);
    }
    let op = [BinOp::Add, BinOp::Sub, BinOp::Mul][usize::try_from(rng.below(3)).expect("idx")];
    Expr::Binary {
        lhs: Box::new(gen_num_expr(rng, joined, depth - 1)),
        op,
        rhs: Box::new(leaf(rng)),
    }
}

fn gen_predicate(rng: &mut Rng, joined: bool, depth: u8) -> Expr {
    let cmp = |rng: &mut Rng| {
        let op = [
            BinOp::Eq,
            BinOp::NotEq,
            BinOp::Lt,
            BinOp::Gt,
            BinOp::LtEq,
            BinOp::GtEq,
        ][usize::try_from(rng.below(6)).expect("idx")];
        Expr::Binary {
            lhs: Box::new(gen_num_expr(rng, joined, 1)),
            op,
            rhs: Box::new(gen_num_expr(rng, joined, 1)),
        }
    };
    if depth == 0 || rng.chance(55) {
        return cmp(rng);
    }
    let op = if rng.chance(60) {
        BinOp::And
    } else {
        BinOp::Or
    };
    Expr::Binary {
        lhs: Box::new(gen_predicate(rng, joined, depth - 1)),
        op,
        rhs: Box::new(cmp(rng)),
    }
}

fn gen_select(
    rng: &mut Rng,
    skeleton: &SelectStatement,
    joined: &SelectStatement,
) -> SelectStatement {
    let use_join = rng.chance(35);
    let mut s = if use_join {
        joined.clone()
    } else {
        skeleton.clone()
    };
    let grouped = rng.chance(25);
    if grouped {
        let (gq, gn) =
            NUM_COLS[usize::try_from(rng.below(if use_join { 3 } else { 2 })).expect("idx")];
        s.group_by = Some(vec![col(gq, gn)]);
        let agg = ["count", "sum", "min", "max"][usize::try_from(rng.below(4)).expect("idx")];
        s.items = vec![
            SelectItem::Expr {
                expr: col(gq, gn),
                alias: None,
            },
            SelectItem::Expr {
                expr: Expr::FunctionCall {
                    name: agg.to_string(),
                    args: vec![gen_num_expr(rng, use_join, 1)],
                },
                alias: Some("agg".to_string()),
            },
        ];
        if rng.chance(30) {
            s.having = Some(Expr::Binary {
                lhs: Box::new(Expr::FunctionCall {
                    name: "count".to_string(),
                    args: vec![col(gq, gn)],
                }),
                op: BinOp::Gt,
                rhs: Box::new(int(1)),
            });
        }
        // Deterministic output order for the grouped shape.
        s.order_by = vec![OrderBy {
            expr: int(1),
            desc: rng.chance(40),
            nulls_first: None,
            collation: None,
        }];
    } else {
        let n_items = 1 + rng.below(3);
        let pool: &[(&str, &str)] = if use_join { COLS } else { &COLS[..3] };
        let mut items = Vec::new();
        for _ in 0..n_items {
            let e = if rng.chance(25) {
                gen_num_expr(rng, use_join, 2)
            } else {
                let (q, n) = pool[usize::try_from(rng.below(pool.len() as u64)).expect("idx")];
                col(q, n)
            };
            items.push(SelectItem::Expr {
                expr: e,
                alias: None,
            });
        }
        s.items = items;
        s.distinct = rng.chance(15);
        if rng.chance(50) {
            s.order_by = vec![OrderBy {
                expr: gen_num_expr(rng, use_join, 1),
                desc: rng.chance(40),
                nulls_first: if rng.chance(20) {
                    Some(rng.chance(50))
                } else {
                    None
                },
                collation: None,
            }];
        }
    }
    if rng.chance(60) {
        s.where_ = Some(gen_predicate(rng, use_join, 2));
    }
    if rng.chance(30) {
        s.limit = Some(spg_sql::ast::LimitExpr::Literal(
            u32::try_from(1 + rng.below(20)).expect("small"),
        ));
    }
    s
}

/// Normalise one leg's answer: rows sorted (order-insensitive — an
/// unpinned tie order is legal), booleans canonicalised, an error
/// collapsed to the token `ERROR` (both legs erroring is agreement;
/// wording may differ legally).
fn canon(rows: Result<Vec<Vec<String>>, String>) -> String {
    match rows {
        Err(_) => "ERROR".to_string(),
        Ok(rs) => {
            let mut lines: Vec<String> = rs
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|c| match c.as_str() {
                            "t" | "true" => "t".to_string(),
                            "f" | "false" => "f".to_string(),
                            other => other.to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join("|")
                })
                .collect();
            lines.sort();
            lines.join("\n")
        }
    }
}

fn embedded_rows(e: &mut spg_engine::Engine, sql: &str) -> Result<Vec<Vec<String>>, String> {
    match e.execute(sql) {
        Err(err) => Err(format!("{err}")),
        Ok(spg_engine::QueryResult::Rows { rows, .. }) => Ok(rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect()
            })
            .collect()),
        Ok(_) => Ok(Vec::new()),
    }
}

fn wire_rows(
    q: Result<suitelib::wireclient::QueryResult, String>,
) -> Result<Vec<Vec<String>>, String> {
    let q = q?;
    if let Some(e) = q.error {
        return Err(e);
    }
    Ok(q.rows)
}

struct Legs {
    engine: spg_engine::Engine,
    simple: suitelib::wireclient::Conn,
    extended: suitelib::wireclient::Conn,
}

impl Legs {
    fn diverges(&mut self, sql: &str) -> Option<String> {
        let a = canon(embedded_rows(&mut self.engine, sql));
        let b = canon(wire_rows(self.simple.simple_query(sql)));
        let c = canon(wire_rows(self.extended.extended_query(sql)));
        if a == b && b == c {
            None
        } else {
            Some(format!(
                "embedded:\n{a}\n-- simple:\n{b}\n-- extended:\n{c}"
            ))
        }
    }
}

/// Clause-dropping shrink: keep removing whichever clause still
/// reproduces the divergence, smallest tree wins.
fn shrink(legs: &mut Legs, mut s: SelectStatement) -> SelectStatement {
    loop {
        let mut candidates: Vec<SelectStatement> = Vec::new();
        if s.where_.is_some() {
            let mut c = s.clone();
            c.where_ = None;
            candidates.push(c);
        }
        if !s.order_by.is_empty() {
            let mut c = s.clone();
            c.order_by = Vec::new();
            candidates.push(c);
        }
        if s.having.is_some() {
            let mut c = s.clone();
            c.having = None;
            candidates.push(c);
        }
        if s.limit.is_some() {
            let mut c = s.clone();
            c.limit = None;
            candidates.push(c);
        }
        if s.distinct {
            let mut c = s.clone();
            c.distinct = false;
            candidates.push(c);
        }
        if s.items.len() > 1 {
            let mut c = s.clone();
            c.items.truncate(1);
            candidates.push(c);
        }
        let mut advanced = false;
        for c in candidates {
            let sql = Statement::Select(c.clone()).to_string();
            if legs.diverges(&sql).is_some() {
                s = c;
                advanced = true;
                break;
            }
        }
        if !advanced {
            return s;
        }
    }
}

#[allow(clippy::too_many_lines)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut seed: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(1);
    let mut count: u64 = 1000;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seed" => {
                seed = args[i + 1].parse().expect("--seed u64");
                i += 2;
            }
            "--count" => {
                count = args[i + 1].parse().expect("--count u64");
                i += 2;
            }
            other => {
                eprintln!("spg-gendiff [--seed N] [--count N] (unknown arg {other})");
                std::process::exit(2);
            }
        }
    }
    println!("spg-gendiff seed={seed} count={count} (replay: --seed {seed})");

    // Skeletons — parsed once, mutated structurally forever after.
    let skeleton = match spg_sql::parser::parse_statement("SELECT t1.a FROM t1") {
        Ok(Statement::Select(s)) => s,
        other => panic!("skeleton parse: {other:?}"),
    };
    let joined =
        match spg_sql::parser::parse_statement("SELECT t1.a FROM t1 JOIN t2 ON t1.a = t2.x") {
            Ok(Statement::Select(s)) => s,
            other => panic!("joined skeleton parse: {other:?}"),
        };

    // Embedded leg.
    let mut engine = spg_engine::Engine::new();
    for sql in SETUP {
        engine.execute(sql).expect("setup on embedded");
    }
    // Wire legs — one REAL server, one connection per protocol.
    let bin = Path::new("target/release/spg-server");
    assert!(bin.exists(), "build spg-server first");
    let tmp = suitelib::proclib::run_tmp_dir("gendiff");
    let _ = std::fs::remove_dir_all(&tmp);
    let mut roster = suitelib::proclib::Roster::new();
    let port = roster
        .spawn_server("gendiff", bin, &tmp, Duration::from_secs(20))
        .expect("server");
    let mut setup_conn = suitelib::wireclient::Conn::connect(port, "gen", "gen").expect("connect");
    for sql in SETUP {
        let r = setup_conn.simple_query(sql).expect("setup on wire");
        assert!(r.error.is_none(), "setup on wire: {:?}", r.error);
    }
    let mut legs = Legs {
        engine,
        simple: setup_conn,
        extended: suitelib::wireclient::Conn::connect(port, "gen", "gen").expect("connect"),
    };

    let mut rng = Rng(seed);
    let mut divergences = 0usize;
    let mut drafted: Vec<String> = Vec::new();
    for n in 0..count {
        let stmt = gen_select(&mut rng, &skeleton, &joined);
        let sql = Statement::Select(stmt.clone()).to_string();
        if let Some(detail) = legs.diverges(&sql) {
            divergences += 1;
            let small = shrink(&mut legs, stmt);
            let small_sql = Statement::Select(small).to_string();
            let mut draft = String::new();
            let _ = writeln!(
                draft,
                "# DRAFT (spg-gendiff seed={seed} n={n}) — three-leg divergence.\n\
                 # Review before adopting; setup mirrors the generator's schema.\n\
                 # Shrunk from: {sql}\n#\n# Divergence detail:"
            );
            for l in detail.lines() {
                let _ = writeln!(draft, "#   {l}");
            }
            let _ = writeln!(draft, "#\n# Offending statement:\n# {small_sql}");
            let path =
                format!("xtests/sqllogictest/corpus/15_regressions/gen_{seed}_{n}.test.draft");
            std::fs::write(&path, draft).expect("write draft");
            eprintln!("DIVERGENCE n={n}: {small_sql}\n  -> {path}");
            drafted.push(path);
        }
        if n > 0 && n % 1000 == 0 {
            println!("  {n}/{count} … {divergences} divergence(s)");
        }
    }
    roster.reap_all();
    let _ = std::fs::remove_dir_all(&tmp);
    println!(
        "spg-gendiff seed={seed}: {count} statements, {divergences} divergence(s){}",
        if drafted.is_empty() {
            String::new()
        } else {
            format!(", drafts: {drafted:?}")
        }
    );
    if divergences > 0 {
        std::process::exit(1);
    }
}
