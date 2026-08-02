//! Round 693 — the range comparison, which closes F36's last shape.
//!
//! `loc BETWEEN 'a' AND 'd'` is not an ordering difference. It returns a
//! different ROW SET from the same SQL: under `en_US.utf8` PG18 returns
//! `apple, Ápple, Banana, cherry`, and byte order drops `Ápple` (0xC3) and
//! `Banana` (0x42 sorts before `a`). That is why this one was worth the
//! design rather than more wiring.
//!
//! Where it landed matters more than that it landed. `binop::compare` takes
//! two values and no column, and its own comment measures it at 35.6 % of
//! self time on a scan — a per-row collation lookup there would have to earn
//! its place against a bench. It is not there. The predicate COMPILER
//! decides once, while compiling: an operand that derives a collation sends
//! that subtree to the tree evaluator, exactly as the enum knife already
//! sends an enum-witnessed comparison. A column that declares nothing never
//! leaves the VM, so the scan pays nothing.
//!
//! Measured on PG18, this is the whole of what a collation reaches:
//! `=`, `<>`, `LIKE`, `IN` and `count(DISTINCT …)` all give byte-equality's
//! answer under a deterministic collation, and only the four ordering
//! operators plus `least`/`greatest` change.
//!
//! Finding the seam took a panic, not a reading: the first version put the
//! hook in `eval_expr`'s Binary arm, which looked like the obvious home and
//! was never entered — a WHERE predicate goes through the compiled VM.

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
    e.execute("CREATE TABLE r693(a TEXT COLLATE \"en_US.utf8\", plain TEXT)")
        .unwrap();
    e.execute(
        "INSERT INTO r693 VALUES ('Zebra','Zebra'),('apple','apple'),('Banana','Banana'),\
         ('Ápple','Ápple'),('cherry','cherry')",
    )
    .unwrap();
}

/// PG18-verified: four rows, and byte order returns two.
#[test]
fn round693_between_returns_the_collations_row_set() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(
        rows(&mut e, "SELECT a FROM r693 WHERE a BETWEEN 'a' AND 'd' ORDER BY a"),
        "apple,Ápple,Banana,cherry"
    );
    assert_eq!(
        rows(&mut e, "SELECT a FROM r693 WHERE a < 'd' ORDER BY a"),
        "apple,Ápple,Banana,cherry"
    );
}

/// All four ordering operators, not just the two BETWEEN lowers to.
#[test]
fn round693_every_ordering_operator() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(
        rows(&mut e, "SELECT a FROM r693 WHERE a > 'd' ORDER BY a"),
        "Zebra"
    );
    assert_eq!(
        rows(&mut e, "SELECT a FROM r693 WHERE a >= 'cherry' ORDER BY a"),
        "cherry,Zebra"
    );
    assert_eq!(
        rows(&mut e, "SELECT a FROM r693 WHERE a <= 'Banana' ORDER BY a"),
        "apple,Ápple,Banana"
    );
}

/// The undeclared column keeps byte order — two rows, not four. Without
/// this the pins above would pass if every column had become en_US.
#[test]
fn round693_an_undeclared_column_keeps_the_byte_row_set() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(
        rows(
            &mut e,
            "SELECT plain FROM r693 WHERE plain BETWEEN 'a' AND 'd' ORDER BY plain"
        ),
        "apple,cherry"
    );
}

/// Equality is untouched, and that is measured rather than assumed: PG18's
/// `en_US.utf8` is deterministic, so `=` compares bytes there too. If a
/// later round routes equality through the collator, this fails.
#[test]
fn round693_equality_and_like_are_unchanged() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(rows(&mut e, "SELECT a FROM r693 WHERE a = 'APPLE'"), "");
    assert_eq!(rows(&mut e, "SELECT a FROM r693 WHERE a = 'apple'"), "apple");
    assert_eq!(rows(&mut e, "SELECT a FROM r693 WHERE a LIKE 'a%'"), "apple");
    assert_eq!(
        rows(&mut e, "SELECT count(DISTINCT a)::text FROM r693"),
        "5"
    );
}

/// `least` / `greatest` follow the ordering operators. PG18 over this
/// column: `least(a,'d')` is `d` and `greatest(a,'d')` is `Zebra`; byte
/// order returns that pair reversed.
#[test]
fn round693_least_and_greatest_follow_the_collation() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(
        rows(
            &mut e,
            "SELECT least(a,'d') || '/' || greatest(a,'d') FROM r693 WHERE a = 'Zebra'"
        ),
        "d/Zebra"
    );
    // And over the column that declares nothing, the byte answer.
    assert_eq!(
        rows(
            &mut e,
            "SELECT least(plain,'d') || '/' || greatest(plain,'d') FROM r693 \
             WHERE plain = 'Zebra'"
        ),
        "Zebra/d"
    );
}
