//! r1042 — a cast on a literal must not cost the index.
//!
//! `WHERE id = 7` sought and `WHERE id = 7::int` scanned, because the
//! seek resolver reads an `Expr::Literal` and a `Cast` node is not one.
//! Measured over the wire on a 400,000-row table, both spellings of the
//! same primary-key lookup:
//!
//! ```text
//! WHERE id = 7          Index Scan   0.08 ms
//! WHERE id = 7::int     Seq Scan     1.86 ms
//! ```
//!
//! That is the shape an ORM writes, the shape `pg_dump` writes, and the
//! shape anyone writes when being explicit — and it had been true for
//! every type since long before the ones that made it visible.
//!
//! What is pinned here is the PLAN, not a duration: a timing assertion
//! on a shared machine pins nothing, and "Index Scan" is the thing the
//! change is actually about.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE cf (id INT PRIMARY KEY, b BYTEA, d DATE, u UUID, t TEXT)")
        .unwrap();
    e.execute(
        "INSERT INTO cf VALUES \
         (7, '\\x0000000000000007', '2026-01-02', \
          '11111111-1111-4111-8111-111111111111', 'x')",
    )
    .unwrap();
    e.execute("CREATE INDEX cf_b ON cf (b)").unwrap();
    e.execute("CREATE INDEX cf_d ON cf (d)").unwrap();
    e.execute("CREATE INDEX cf_u ON cf (u)").unwrap();
    e
}

fn plan(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// The cast spelling and the bare one plan the same way, on every type
/// whose cast target is catalog-independent.
#[test]
fn r1042_a_cast_on_a_literal_still_seeks() {
    let mut e = engine();
    for (bare, cast) in [
        (
            "SELECT id FROM cf WHERE id = 7",
            "SELECT id FROM cf WHERE id = 7::int",
        ),
        (
            "SELECT id FROM cf WHERE b = '\\x0000000000000007'",
            "SELECT id FROM cf WHERE b = '\\x0000000000000007'::bytea",
        ),
        (
            "SELECT id FROM cf WHERE d = '2026-01-02'",
            "SELECT id FROM cf WHERE d = '2026-01-02'::date",
        ),
        (
            "SELECT id FROM cf WHERE u = '11111111-1111-4111-8111-111111111111'",
            "SELECT id FROM cf WHERE u = '11111111-1111-4111-8111-111111111111'::uuid",
        ),
    ] {
        let pb = plan(&mut e, &format!("EXPLAIN {bare}"));
        let pc = plan(&mut e, &format!("EXPLAIN {cast}"));
        assert!(
            pb[0].contains("Index Scan") || pb[0].contains("Index Only Scan"),
            "the bare form stopped seeking: {pb:?}"
        );
        assert_eq!(
            pb, pc,
            "cast planned differently from bare:\n{bare}\n{cast}"
        );
        // And the answer is the same, which is the part a plan cannot say.
        assert_eq!(rows(&mut e, bare), rows(&mut e, cast));
        assert_eq!(rows(&mut e, bare), alloc_vec_7());
    }
}

fn alloc_vec_7() -> Vec<String> {
    vec!["7".to_string()]
}

/// Arithmetic between literals folds too, which is what makes
/// `WHERE id = 3 + 4` a seek rather than a scan.
#[test]
fn r1042_arithmetic_between_literals_folds() {
    let mut e = engine();
    let p = plan(&mut e, "EXPLAIN SELECT id FROM cf WHERE id = 3 + 4");
    assert!(
        p[0].contains("Index Scan") || p[0].contains("Index Only Scan"),
        "{p:?}"
    );
    assert!(
        p.iter().any(|l| l.contains("Index Cond: (id = 7)")),
        "the fold did not reach the plan: {p:?}"
    );
}

/// The half this pass got wrong the first time.
///
/// `'u'::regclass` means whatever the CATALOG says it means, and this
/// pass evaluates against an empty context. Folded, it came back as the
/// text `u` and twenty-six catalog tests went red comparing an oid to a
/// string. The failure mode was not a refusal — it was a fold that
/// SUCCEEDED and was wrong, which is why the cast targets are listed one
/// by one rather than allowed by default.
#[test]
fn r1042_a_catalog_dependent_cast_is_not_folded() {
    let mut e = engine();
    // The plan text still carries the cast: nothing folded it away.
    let p = plan(
        &mut e,
        "EXPLAIN SELECT id FROM cf WHERE id = 'cf'::regclass",
    );
    assert!(
        p.iter().any(|l| l.contains("regclass")),
        "a catalog-dependent cast was folded: {p:?}"
    );
    // And the thing those twenty-six tests do keeps working.
    assert_eq!(
        rows(
            &mut e,
            "SELECT count(*) FROM pg_attribute WHERE attrelid = 'cf'::regclass AND attnum > 0"
        ),
        vec!["5".to_string()]
    );
}

/// An expression that raises keeps raising from where it did. The fold
/// declines rather than moving the error to prepare time.
#[test]
fn r1042_a_raising_constant_is_left_alone() {
    let mut e = engine();
    let err = e
        .execute("SELECT id FROM cf WHERE id = 1 / 0")
        .expect_err("division by zero must still raise");
    let msg = format!("{err}");
    assert!(msg.contains("division by zero"), "{msg}");
}
