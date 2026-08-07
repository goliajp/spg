//! Round 691 — an explicit `COLLATE` on an ORDER BY key is honoured.
//!
//! This one needed no collation DERIVATION, which is what the two remaining
//! residuals (`BETWEEN`, `ORDER BY upper()`) do need: the name is written in
//! the query. What it needed was somewhere to PUT the name. The expression
//! parser consumed the clause and dropped it, so by the time the sort ran
//! there was nothing left to honour.
//!
//! It went on `ast::OrderBy`, beside `desc` and `nulls_first`, because at an
//! ORDER BY key a collation is ordering information and nothing downstream
//! of the sort wants it. The alternative — an `Expr::Collate` variant —
//! would have put a new arm on `eval_expr`, which this repo has measured to
//! overflow the debug stack.
//!
//! The parser reaches it through a save/restore channel, the same shape it
//! already uses for `pending_sample_preds`, and only while an ORDER BY key
//! is being parsed. Everywhere else an unperformable collation still errors
//! — see the last test. Accepting one at a COMPARISON and ignoring it is
//! precisely the defect F36 exists to close, and a comparison cannot be
//! fixed by carrying a name: `binop::compare` takes two values, and which
//! operand's collation applies is a derivation question SPG does not model.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seed(e: &mut Engine) {
    // No COLLATE on the column: the explicit clause is the only source of
    // one, so these pins cannot pass on the strength of round 688's work.
    e.execute("CREATE TABLE x691(plain TEXT, other TEXT COLLATE \"C\")")
        .unwrap();
    e.execute(
        "INSERT INTO x691 VALUES ('Zebra','Zebra'),('apple','apple'),\
         ('Banana','Banana'),('Ápple','Ápple'),('cherry','cherry')",
    )
    .unwrap();
}

const EN_US: &str = "apple,Ápple,Banana,cherry,Zebra";
const BYTES: &str = "Banana,Zebra,apple,cherry,Ápple";

/// Both PG18-verified on a column that declares nothing.
#[test]
fn round691_an_explicit_collate_orders_the_key() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(
        rows(
            &mut e,
            "SELECT plain FROM x691 ORDER BY plain COLLATE \"en_US.utf8\""
        ),
        EN_US
    );
    // And without it the same column keeps byte order.
    assert_eq!(rows(&mut e, "SELECT plain FROM x691 ORDER BY plain"), BYTES);
}

/// An explicit `C` is still the byte order it has always been — the clause
/// was accepted as a no-op long before it meant anything, and it must not
/// have started meaning something else.
#[test]
fn round691_an_explicit_c_is_byte_order() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(
        rows(
            &mut e,
            "SELECT plain FROM x691 ORDER BY plain COLLATE \"C\""
        ),
        BYTES
    );
}

/// The explicit name beats the column's declaration. PG's rule, and the only
/// way to ask for an order the column does not declare.
#[test]
fn round691_the_explicit_name_overrides_the_columns() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(rows(&mut e, "SELECT other FROM x691 ORDER BY other"), BYTES);
    assert_eq!(
        rows(
            &mut e,
            "SELECT other FROM x691 ORDER BY other COLLATE \"en_US.utf8\""
        ),
        EN_US
    );
}

/// DESC applies to the collated order, not to a byte order underneath it.
#[test]
fn round691_desc_reverses_the_explicit_order() {
    let mut e = Engine::new();
    seed(&mut e);
    let want: Vec<&str> = EN_US.split(',').rev().collect();
    assert_eq!(
        rows(
            &mut e,
            "SELECT plain FROM x691 ORDER BY plain COLLATE \"en_US.utf8\" DESC"
        ),
        want.join(",")
    );
}

/// A name this build cannot perform is REFUSED rather than dropped. The
/// parser refused it before there was anywhere to put it; now the engine
/// does, and the honesty is the point, not the layer.
#[test]
fn round691_an_unperformable_name_is_refused() {
    let mut e = Engine::new();
    seed(&mut e);
    let err = e
        .execute("SELECT plain FROM x691 ORDER BY plain COLLATE \"nonesuch_XX\"")
        .expect_err("must not silently ignore the clause");
    assert!(
        alloc_fmt(&err).contains("nonesuch_XX"),
        "error should name it: {err}"
    );
}

/// Outside an ORDER BY key nothing changed: a locale collation at a
/// COMPARISON still errors. PG answers those; SPG saying so is the honest
/// gap, and it is recorded in `docs/COLLATION_RFC.md` §4f as derivation
/// work, not wiring.
#[test]
fn round691_a_comparison_still_refuses_a_locale_collation() {
    let mut e = Engine::new();
    seed(&mut e);
    assert!(
        e.execute("SELECT plain FROM x691 WHERE plain COLLATE \"en_US.utf8\" < 'z'")
            .is_err()
    );
}

fn alloc_fmt(e: &spg_engine::EngineError) -> String {
    format!("{e}")
}
