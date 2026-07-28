//! v7.39 (round 594) — the regex pattern was parsed again for every row.
//!
//! Before anything could be attacked, the target this round was sent at had
//! to be re-measured, and it evaporated. `sum(id) OVER (PARTITION BY g)` was
//! recorded at 26.3x, and PG's plan for it has no WindowAgg node at all: the
//! benchmark wrapped the window in `count(*)`, which never reads the window
//! column, so PG dropped the window and SPG was being compared against a
//! bare parallel scan. The same is true of the `lag()` query rounds 592 and
//! 593 reported — their 18.7x / 14.1x / 13.4x are not window-vs-window
//! numbers. Forcing PG to actually compute the window, on 500k rows:
//!
//!     sum(id) OVER (PARTITION BY g)        SPG 165.79   PG  80.98   2.05x
//!     sum(id) OVER (PARTITION BY g ORDER)  SPG 304.65   PG 168.90   1.80x
//!     row_number() OVER (ORDER BY id)      SPG  75.12   PG  62.82   1.20x
//!     lag(id) OVER (ORDER BY id)           SPG 106.14   PG  90.62   1.17x
//!     sum(lag(id) OVER (ORDER BY id))      SPG  85.57   PG  87.87   0.97x
//!     (plain sorted derived table)         SPG  55.56   PG  27.61   2.01x
//!
//! So the window path is 0.97-2.05x, and what is left of it tracks the sort
//! underneath. The 26.3x was never there.
//!
//! That left the regex filter as the worst measured shape, and it is real —
//! PG's plan is a genuine parallel scan with the filter. 500k rows:
//!
//!     s ~ '^row1234[0-9]$'      350.36 ->  22.60 ms   PG 34.52
//!     s ~ 'row1234'             254.51 ->  21.84      PG 39.23
//!     s ~* '^ROW1234[0-9]$'     408.90 ->  21.71      PG 34.07
//!     s !~ '^row1234[0-9]$'     345.59 ->  24.40      PG 38.24
//!     regexp_like(s, '^row…$')  342.70 ->  23.49      PG 34.73
//!     s ~ ('^row' || '1234…')   358.41 -> 358.41      (unchanged, and right)
//!
//! Every spelling cost the same before, which is the shape of a per-row
//! compile rather than a slow matcher: `s ~ 'p'` lowers to
//! `regexp_like(s, 'p')`, and that function parsed the pattern into a tree
//! for every row it was asked about. PG keeps a cache of compiled patterns.
//!
//! SPG has somewhere better than a cache to put it. The compiled-predicate
//! program already holds `Step::Like`'s pattern as a compile PRODUCT, which
//! is the same idea without a cache's "forgot to pass the memo" failure
//! mode, so `Step::Regex` holds the compiled tree — including the decision
//! about the anchored-dot-run shortcut, which used to be re-derived per row.
//! A pattern that is not a literal stays on the interpreter, because there
//! it really can differ from row to row: the last line above is that case,
//! and it is unchanged on purpose.
//!
//! What the pins are for. The compiled step has to answer exactly what the
//! interpreter answers — including for an operand that is not text, where it
//! evaluates the whole call the interpreter's way rather than inventing a
//! verdict. All 24 shapes here were checked against live PG18 and matched.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn ids(e: &mut Engine, sql: &str) -> Vec<String> {
    vals(e, sql)
}

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE rx (id INT, s TEXT, n INT)").unwrap();
    e.execute(
        "INSERT INTO rx VALUES (1,'abc',10),(2,'ABC',20),(3,'a1b2c3',30),(4,'',40),(5,NULL,50),\
         (6,'aaa',60),(7,'x.y',70),(8,'line1\nline2',80),(9,'  pad  ',90),(10,'ab',100),\
         (11,'abab',110),(12,'a+b',120)",
    )
    .unwrap();
    e
}

/// The four operator spellings, all of which lower to the same call.
#[test]
fn round594_operator_spellings() {
    let mut e = seed();
    assert_eq!(
        ids(&mut e, "SELECT id FROM rx WHERE s ~ '^a' ORDER BY id"),
        vec!["1", "3", "6", "10", "11", "12"]
    );
    assert_eq!(
        ids(&mut e, "SELECT id FROM rx WHERE s ~ '^abc$' ORDER BY id"),
        vec!["1"]
    );
    assert_eq!(
        ids(&mut e, "SELECT id FROM rx WHERE s ~* '^abc$' ORDER BY id"),
        vec!["1", "2"],
        "case-insensitive folds at compile time now"
    );
    assert_eq!(
        ids(&mut e, "SELECT id FROM rx WHERE s !~ '^a' ORDER BY id"),
        vec!["2", "4", "7", "8", "9"],
        "a NULL subject is neither matched nor negated into the answer"
    );
    assert_eq!(
        ids(&mut e, "SELECT id FROM rx WHERE s !~* 'ABC' ORDER BY id"),
        vec!["3", "4", "6", "7", "8", "9", "10", "11", "12"]
    );
    assert_eq!(
        ids(
            &mut e,
            "SELECT id FROM rx WHERE regexp_like(s, '^ABC$', 'i') ORDER BY id"
        ),
        vec!["1", "2"],
        "the function spelling with an explicit flag"
    );
}

/// The engine's own features have to survive being compiled early:
/// backreferences, classes, alternation, escapes, and the anchored-dot-run
/// shortcut whose decision moved to compile time.
#[test]
fn round594_pattern_features() {
    let mut e = seed();
    assert_eq!(
        ids(&mut e, "SELECT id FROM rx WHERE s ~ '[0-9]{1,2}' ORDER BY id"),
        vec!["3", "8"]
    );
    assert_eq!(
        ids(&mut e, "SELECT id FROM rx WHERE s ~ '(ab)+$' ORDER BY id"),
        vec!["10", "11"]
    );
    assert_eq!(
        ids(&mut e, r"SELECT id FROM rx WHERE s ~ '(a)\1' ORDER BY id"),
        vec!["6"],
        "a backreference still forces the capturing matcher"
    );
    assert_eq!(
        ids(&mut e, "SELECT id FROM rx WHERE s ~ 'line1.line2' ORDER BY id"),
        vec!["8"],
        "`.` matches a newline, as PG's does"
    );
    assert_eq!(
        ids(&mut e, r"SELECT id FROM rx WHERE s ~ 'a\+b' ORDER BY id"),
        vec!["12"]
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM rx WHERE s ~ ''"),
        vec!["11"],
        "the empty pattern matches every non-NULL subject"
    );
    assert_eq!(
        ids(&mut e, "SELECT id FROM rx WHERE s ~ '^.*$' ORDER BY id"),
        vec!["1", "2", "3", "4", "6", "7", "8", "9", "10", "11", "12"],
        "the anchored-dot-run shortcut, decided once"
    );
    assert_eq!(
        ids(&mut e, "SELECT id FROM rx WHERE s ~ '^...$' ORDER BY id"),
        vec!["1", "2", "6", "7", "12"],
        "and its bounded form"
    );
    assert_eq!(
        ids(
            &mut e,
            "SELECT id FROM rx WHERE s ~ '[[:alpha:]]+' ORDER BY id"
        ),
        vec!["1", "2", "3", "6", "7", "8", "9", "10", "11", "12"]
    );
    assert_eq!(
        vals(&mut e, "SELECT substring(s FROM 'a.*?c') FROM rx WHERE id = 3"),
        vec!["a1b2c"],
        "a lazy quantifier through a different entry point"
    );
}

/// A pattern that is not a literal cannot be compiled once, and must keep
/// answering the same.
#[test]
fn round594_non_literal_patterns_keep_the_interpreter() {
    let mut e = seed();
    assert_eq!(
        ids(&mut e, "SELECT id FROM rx WHERE s ~ ('^' || 'a') ORDER BY id"),
        vec!["1", "3", "6", "10", "11", "12"],
        "a concatenated pattern gives the same answer as the literal one"
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM rx a JOIN rx b ON a.s ~ b.s"),
        vec!["25"],
        "a pattern that genuinely differs per row"
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM rx WHERE s ~ NULL"),
        vec!["0"],
        "a NULL pattern matches nothing"
    );
}

/// Where the match appears changes which path runs it — filter, projection,
/// CASE, and beside other predicates.
#[test]
fn round594_every_position_agrees() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT id, s ~ '^a' FROM rx ORDER BY id"),
        vec![
            "1|true", "2|false", "3|true", "4|false", "5|NULL", "6|true", "7|false", "8|false",
            "9|false", "10|true", "11|true", "12|true",
        ],
        "projected, not filtered — and NULL stays NULL"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, CASE WHEN s ~ '^a' THEN 'y' ELSE 'n' END FROM rx ORDER BY id"
        ),
        vec![
            "1|y", "2|n", "3|y", "4|n", "5|n", "6|y", "7|n", "8|n", "9|n", "10|y", "11|y", "12|y",
        ]
    );
    assert_eq!(
        ids(&mut e, "SELECT id FROM rx WHERE s ~ '^a' AND n > 20 ORDER BY id"),
        vec!["3", "6", "10", "11", "12"]
    );
    assert_eq!(
        ids(&mut e, "SELECT id FROM rx WHERE s ~ '^x' OR n = 10 ORDER BY id"),
        vec!["1", "7"]
    );
    assert_eq!(
        ids(
            &mut e,
            "SELECT id FROM rx WHERE n::TEXT ~ '^[0-9]0$' ORDER BY id"
        ),
        vec!["1", "2", "3", "4", "5", "6", "7", "8", "9"],
        "the subject is a cast, not a bare column"
    );
}

/// At a size where the compile used to be paid per row, the answer has to be
/// the one the interpreter gives.
#[test]
fn round594_scale_agrees_with_the_interpreter() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT, s TEXT)").unwrap();
    e.execute("INSERT INTO big SELECT gg, 'row' || gg FROM generate_series(1, 20000) gg")
        .unwrap();
    // Literal pattern (compiled once) against the same pattern built by
    // concatenation (interpreted per row) — the same answer, both ways.
    let lit = vals(
        &mut e,
        "SELECT count(*) FROM big WHERE s ~ '^row1234[0-9]$'",
    );
    let built = vals(
        &mut e,
        "SELECT count(*) FROM big WHERE s ~ ('^row1234' || '[0-9]$')",
    );
    assert_eq!(lit, built);
    assert_eq!(lit, vec!["10"]);
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM big WHERE s !~ '^row1234[0-9]$'"),
        vec!["19990"]
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM big WHERE s ~* '^ROW1$'"),
        vec!["1"]
    );
}
