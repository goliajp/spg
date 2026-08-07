//! v7.39 (round 311, V32) — `pg_get_constraintdef(oid, true)`.
//!
//! The ledger recorded this as "the default form already matches PG
//! byte-for-byte, only the pretty form is missing". Measuring nine
//! shapes found three where the DEFAULT form differed too, so the
//! pretty renderer would have been built on a base that was itself off:
//!
//!   * an `AND` / `OR` chain nesting to the LEFT is one chain and prints
//!     flat — `(a) AND (b) AND (c)`, not `((a) AND (b)) AND (c)`. An
//!     explicitly right-nested one keeps its grouping;
//!   * a unary minus is written with a space: `(- a)`;
//!   * a cast parenthesises its OPERAND, not itself: `(a)::text`.
//!
//! Those live in `Expr`'s Display — the round-trip-safe rendering every
//! deparse consumer shares — so they are fixed there rather than patched
//! into this one function.
//!
//! The pretty rules were then read off 37 shapes of live PG 18.4. They
//! are not plain precedence minimisation:
//!
//!   * the boolean layer follows precedence (an OR under an AND keeps
//!     parens, an AND under an OR does not, a comparison under either
//!     does not) and an associative chain flattens completely, even
//!     where the source nested it right;
//!   * but an operand of a COMPARISON keeps its parens whenever it is an
//!     operator expression — `(a + b) > 10` — while the same child under
//!     an arithmetic parent follows precedence: `(a + b * c) > 10`;
//!   * a sign always keeps its parens under an operator: `(- a) + b`;
//!   * `NOT` under `NOT` keeps them, at equal precedence.

use spg_engine::{Engine, QueryResult};

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE p32 (id int PRIMARY KEY)").unwrap();
    e.execute(
        "CREATE TABLE c32 (a int, b int, c int, d int, t text, qty int, code text,
           pid int REFERENCES p32(id),
           CONSTRAINT ck_and   CHECK (qty < 100 AND code IS NOT NULL),
           CONSTRAINT ck_arith CHECK (a + b * c > 10),
           CONSTRAINT ck_mix   CHECK ((a > 1 OR b > 2) AND c > 3),
           CONSTRAINT ck_not   CHECK (NOT (a > 1)),
           CONSTRAINT ck_paren CHECK ((a + b) * c > 10),
           CONSTRAINT k01 CHECK (a + b > 10),
           CONSTRAINT k04 CHECK (NOT (a > 1 AND b > 2)),
           CONSTRAINT k05 CHECK (a > 1 AND (b > 2 OR c > 3)),
           CONSTRAINT k06 CHECK (-a > 1),
           CONSTRAINT k07 CHECK (a::text = t),
           CONSTRAINT k09 CHECK ((a > 1 AND b > 2) OR c > 3),
           CONSTRAINT k13 CHECK ((a + b) > (c * 2)),
           CONSTRAINT k14 CHECK (a > 1),
           CONSTRAINT n01 CHECK (a > 1 AND (b > 2 AND c > 3)),
           CONSTRAINT n02 CHECK ((a > 1 AND b > 2) AND c > 3),
           CONSTRAINT n03 CHECK (a > 1 OR (b > 2 OR c > 3)),
           CONSTRAINT n04 CHECK (a > 1 AND b > 2 AND c > 3 AND d > 4),
           CONSTRAINT n05 CHECK ((a + b)::text = t),
           CONSTRAINT n07 CHECK (-(a + b) > 1),
           CONSTRAINT n08 CHECK (- a + b > 1),
           CONSTRAINT n09 CHECK (a > 1 AND NOT b > 2),
           CONSTRAINT n10 CHECK (NOT NOT a > 1),
           CONSTRAINT n12 CHECK (length(t)::text = t),
           CONSTRAINT n14 CHECK (a - b - c > 1),
           CONSTRAINT n15 CHECK (a - (b - c) > 1))",
    )
    .unwrap();
    e
}

fn defs(e: &mut Engine, pretty: bool) -> Vec<(String, String)> {
    let sql = if pretty {
        "SELECT conname, pg_get_constraintdef(oid, true) FROM pg_constraint \
         WHERE contype='c' ORDER BY conname"
    } else {
        "SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint \
         WHERE contype='c' ORDER BY conname"
    };
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                (
                    spg_engine::eval::value_to_text(&r.values[0]),
                    spg_engine::eval::value_to_text(&r.values[1]),
                )
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn check(got: &[(String, String)], name: &str, want: &str) {
    let found = got
        .iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("{name} missing"));
    assert_eq!(found.1, want, "{name}");
}

/// Every default-form rendering, byte for byte against PG 18.4.
#[test]
fn the_default_form_matches_pg() {
    let mut e = fixture();
    let d = defs(&mut e, false);
    // The three this round corrected.
    check(&d, "n02", "CHECK (((a > 1) AND (b > 2) AND (c > 3)))");
    check(
        &d,
        "n04",
        "CHECK (((a > 1) AND (b > 2) AND (c > 3) AND (d > 4)))",
    );
    check(&d, "k06", "CHECK (((- a) > 1))");
    check(&d, "k07", "CHECK (((a)::text = t))");
    check(&d, "n05", "CHECK ((((a + b))::text = t))");
    check(&d, "n12", "CHECK (((length(t))::text = t))");
    // Right nesting is a different grouping and keeps its parens.
    check(&d, "n01", "CHECK (((a > 1) AND ((b > 2) AND (c > 3))))");
    check(&d, "n03", "CHECK (((a > 1) OR ((b > 2) OR (c > 3))))");
    // The ones that already agreed must not have moved.
    check(&d, "ck_and", "CHECK (((qty < 100) AND (code IS NOT NULL)))");
    check(&d, "ck_arith", "CHECK (((a + (b * c)) > 10))");
    check(&d, "ck_paren", "CHECK ((((a + b) * c) > 10))");
    check(&d, "k04", "CHECK ((NOT ((a > 1) AND (b > 2))))");
    check(&d, "k13", "CHECK (((a + b) > (c * 2)))");
    check(&d, "k14", "CHECK ((a > 1))");
    check(&d, "n07", "CHECK (((- (a + b)) > 1))");
    check(&d, "n08", "CHECK ((((- a) + b) > 1))");
    check(&d, "n10", "CHECK ((NOT (NOT (a > 1))))");
    check(&d, "n15", "CHECK (((a - (b - c)) > 1))");
}

#[test]
fn the_pretty_form_matches_pg() {
    let mut e = fixture();
    let d = defs(&mut e, true);
    // Boolean layer: precedence, and full flattening.
    check(&d, "ck_and", "CHECK (qty < 100 AND code IS NOT NULL)");
    check(&d, "ck_mix", "CHECK ((a > 1 OR b > 2) AND c > 3)");
    check(&d, "k05", "CHECK (a > 1 AND (b > 2 OR c > 3))");
    check(&d, "k09", "CHECK (a > 1 AND b > 2 OR c > 3)");
    check(&d, "n01", "CHECK (a > 1 AND b > 2 AND c > 3)");
    check(&d, "n02", "CHECK (a > 1 AND b > 2 AND c > 3)");
    check(&d, "n03", "CHECK (a > 1 OR b > 2 OR c > 3)");
    check(&d, "n04", "CHECK (a > 1 AND b > 2 AND c > 3 AND d > 4)");
    // NOT: loses parens over a comparison, keeps them over a chain and
    // over another NOT.
    check(&d, "ck_not", "CHECK (NOT a > 1)");
    check(&d, "k04", "CHECK (NOT (a > 1 AND b > 2))");
    check(&d, "n09", "CHECK (a > 1 AND NOT b > 2)");
    check(&d, "n10", "CHECK (NOT (NOT a > 1))");
    // A comparison's operator operand keeps its parens; the same child
    // under an arithmetic parent follows precedence.
    check(&d, "k01", "CHECK ((a + b) > 10)");
    check(&d, "k13", "CHECK ((a + b) > (c * 2))");
    check(&d, "ck_arith", "CHECK ((a + b * c) > 10)");
    check(&d, "ck_paren", "CHECK (((a + b) * c) > 10)");
    check(&d, "n14", "CHECK ((a - b - c) > 1)");
    check(&d, "n15", "CHECK ((a - (b - c)) > 1)");
    // A sign keeps its parens under an operator.
    check(&d, "k06", "CHECK ((- a) > 1)");
    check(&d, "n07", "CHECK ((- (a + b)) > 1)");
    check(&d, "n08", "CHECK (((- a) + b) > 1)");
    // A cast is compound exactly when what it casts is.
    check(&d, "k07", "CHECK (a::text = t)");
    check(&d, "n12", "CHECK (length(t)::text = t)");
    check(&d, "n05", "CHECK (((a + b)::text) = t)");
    // Nothing to drop.
    check(&d, "k14", "CHECK (a > 1)");
}

/// PG renders the other constraint kinds identically either way, so the
/// second argument must not disturb them.
#[test]
fn only_check_constraints_differ_between_the_two_forms() {
    let mut e = fixture();
    let sql = "SELECT conname, pg_get_constraintdef(oid), pg_get_constraintdef(oid, true) \
               FROM pg_constraint WHERE contype <> 'c' ORDER BY conname";
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert!(!rows.is_empty(), "expected PK / FK rows");
            for r in &rows {
                let plain = spg_engine::eval::value_to_text(&r.values[1]);
                let pretty = spg_engine::eval::value_to_text(&r.values[2]);
                assert_eq!(
                    plain,
                    pretty,
                    "{}",
                    spg_engine::eval::value_to_text(&r.values[0])
                );
            }
        }
        other => panic!("{other:?}"),
    }
}

/// Display stays round-trip safe: the default text re-parses, and
/// re-rendering it is a fixed point. That is the property every other
/// deparse consumer relies on, and the reason these fixes went into
/// Display rather than into the constraint function.
#[test]
fn the_default_rendering_still_round_trips() {
    let mut e = fixture();
    for (name, def) in defs(&mut e, false) {
        let inner = def
            .strip_prefix("CHECK (")
            .and_then(|s| s.strip_suffix(')'))
            .unwrap_or_else(|| panic!("{name}: {def}"));
        let ast = spg_sql::parser::parse_expression(inner)
            .unwrap_or_else(|x| panic!("{name}: {inner} did not re-parse: {x:?}"));
        assert_eq!(ast.to_string(), inner, "{name} is not a fixed point");
    }
}
