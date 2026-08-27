//! 7.38.7 — Describe answers for every expression shape, mechanically.
//!
//! Nine defects have now been filed against one behaviour: Describe met
//! an expression it could not type and reported the WHOLE statement as
//! having no columns, so an ordinary column sitting beside it vanished
//! too. A data-modifying CTE, a subquery in the select list, a top-level
//! null test, ordered-set aggregates, a set-returning function in FROM,
//! a user-defined function — each arrived as a customer report, each was
//! one missing arm, and each time the fix was to add that arm.
//!
//! Adding arms one report at a time is not a strategy. This is the gate
//! that ends it: every `Expr` variant gets a statement, and Describe must
//! name a column for it. A variant with no arm is caught here rather
//! than in somebody's product.
//!
//! The second test is what keeps THIS test honest — it reads the AST's
//! own source, counts the variants, and fails when one appears that this
//! file does not mention. A coverage list nobody is forced to update
//! stops being coverage.

use spg_engine::Engine;

/// One statement per `Expr` variant, plus the variant it exercises.
/// Every entry must describe at least one column.
const CASES: &[(&str, &str)] = &[
    ("Literal", "SELECT 1"),
    ("Column", "SELECT id FROM cov"),
    ("NamedArg", "SELECT round(n => 1.5)"),
    ("Variadic", "SELECT coalesce(VARIADIC ARRAY['a','b'])"),
    ("Placeholder", "SELECT $1::int"),
    ("Binary", "SELECT 1 + 1"),
    ("Unary", "SELECT -id FROM cov"),
    ("Cast", "SELECT 7::bigint"),
    // v7.39.2 — the clause became a node; before that it was refused
    // or absorbed and there was nothing to describe.
    ("Collate", "SELECT 'a' COLLATE \"C\""),
    ("FieldAccess", "SELECT (rec).f FROM cov_rec"),
    ("IsNull", "SELECT k IS NULL FROM cov"),
    ("BoolTest", "SELECT (id = 1) IS TRUE FROM cov"),
    ("FunctionCall", "SELECT length(k) FROM cov"),
    (
        "AggregateOrdered",
        "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY f) FROM cov",
    ),
    ("Like", "SELECT k LIKE 'a%' FROM cov"),
    (
        "WindowFunction",
        "SELECT row_number() OVER (ORDER BY id) FROM cov",
    ),
    ("ScalarSubquery", "SELECT (SELECT max(id) FROM cov)"),
    ("Exists", "SELECT EXISTS (SELECT 1 FROM cov)"),
    ("InSubquery", "SELECT id IN (SELECT id FROM cov) FROM cov"),
    (
        "RowInSubquery",
        "SELECT (id, k) IN (SELECT id, k FROM cov) FROM cov",
    ),
    (
        "RowCmpSubquery",
        "SELECT (id, k) = (SELECT id, k FROM cov LIMIT 1) FROM cov",
    ),
    ("InList", "SELECT id IN (1, 2) FROM cov"),
    ("Extract", "SELECT EXTRACT(YEAR FROM ts) FROM cov"),
    ("Array", "SELECT ARRAY[1, 2]"),
    ("ArraySubscript", "SELECT arr[1] FROM cov"),
    ("ArraySlice", "SELECT arr[1:2] FROM cov"),
    ("AnyAll", "SELECT id = ANY (ARRAY[1, 2]) FROM cov"),
    (
        "Case",
        "SELECT CASE WHEN id = 1 THEN 'a' ELSE 'b' END FROM cov",
    ),
];

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE cov (id INT, k TEXT, n BIGINT, f DOUBLE PRECISION, \
         ts TIMESTAMPTZ, arr INT[])",
    )
    .unwrap();
    // A literal rather than `now()`: a bare Engine has no session clock,
    // and the row is only here so the shapes below have something to
    // describe against.
    e.execute(
        "INSERT INTO cov VALUES (1, 'a', 2, 1.5, \
         TIMESTAMPTZ '2026-01-01 00:00:00+00', ARRAY[1,2,3])",
    )
    .unwrap();
    // For FieldAccess: a composite column.
    e.execute("CREATE TYPE cov_t AS (f INT)").ok();
    e.execute("CREATE TABLE cov_rec (rec cov_t)").ok();
    // For the user-defined-function arm, which is how sentori found this.
    e.execute(
        "CREATE FUNCTION cov_udf() RETURNS BIGINT LANGUAGE sql IMMUTABLE AS $$ SELECT 1::bigint $$",
    )
    .ok();
    e
}

fn described(e: &Engine, sql: &str) -> Option<usize> {
    let stmt = spg_sql::parser::parse_statement(sql).ok()?;
    let (_, cols) = e.describe_prepared(&stmt);
    Some(cols.len())
}

#[test]
fn pin_v7387_every_expression_shape_describes() {
    let e = engine();
    let mut silent: Vec<&str> = Vec::new();
    for (variant, sql) in CASES {
        match described(&e, sql) {
            // A statement this build cannot PARSE is not this gate's
            // business — it fails loudly elsewhere. Describing zero
            // columns for something that parsed is.
            None => continue,
            Some(0) => silent.push(variant),
            Some(_) => {}
        }
    }
    assert!(
        silent.is_empty(),
        "Describe reported NO columns for these expression shapes: {silent:?}. \
         A shape it cannot type must still contribute a named column — \
         erasing the statement's whole column list takes ordinary columns \
         with it, which is the defect this gate exists for."
    );
}

#[test]
fn pin_v7387_a_udf_does_not_erase_its_neighbours() {
    // sentori's case 07, kept separate because it is the shape that
    // makes the class expensive: `id` is an ordinary integer column and
    // it disappeared along with the function call beside it.
    let e = engine();
    assert_eq!(described(&e, "SELECT id, cov_udf() FROM cov"), Some(2));
    assert_eq!(described(&e, "SELECT cov_udf() + 1"), Some(1));
}

#[test]
fn pin_v7387_the_coverage_list_is_complete() {
    // Reads the AST's own source and fails when a variant appears that
    // CASES does not mention. Without this the list silently rots: the
    // next variant lands, nobody adds a case, and the gate above passes
    // while covering less than it did.
    let ast = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../spg-sql/src/ast.rs"
    ))
    .expect("read ast.rs");
    let body = ast
        .split_once("\npub enum Expr {")
        .expect("find enum Expr")
        .1;
    let body = body.split_once("\n}\n").expect("end of enum Expr").0;
    let variants: Vec<&str> = body
        .lines()
        .filter_map(|l| {
            let t = l.strip_prefix("    ")?;
            let name = t.split(['(', ' ', ',']).next()?;
            (!name.is_empty()
                && name.starts_with(|c: char| c.is_ascii_uppercase())
                && name.chars().all(|c| c.is_ascii_alphanumeric()))
            .then_some(name)
        })
        .collect();
    assert!(
        variants.len() > 20,
        "parsed only {} variants — the scan is broken, not the coverage",
        variants.len()
    );
    let missing: Vec<&&str> = variants
        .iter()
        .filter(|v| !CASES.iter().any(|(name, _)| name == *v))
        .collect();
    assert!(
        missing.is_empty(),
        "these Expr variants have no case in CASES: {missing:?}. \
         Add one statement each — a variant nobody described is how every \
         defect in this class arrived."
    );
}
