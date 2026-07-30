//! v7.39 (round 631, F33) — the sign columns, and what a picture that asks
//! for no number does with them.
//!
//! Round 630 diagnosed this and could not fix it in one move: `to_char(1,
//! 'MI')` took the field-overflow branch — one integer digit against zero
//! slots — and returned from inside it, before the sign columns are applied
//! at the end of the function, so the sign was lost. Exempting the branch
//! ALONE was measured to be worse: the body renderer then printed digits
//! with no slots to sit in, and `to_char(1,'B')` answered `1`. Both halves
//! land together here, keyed on one question — does the picture ask for a
//! number at all? Digit slots, or a decimal separator, which counts even
//! with nothing around it.
//!
//! Measuring each sign element with AND without slots then showed three
//! that were not what they looked like:
//!
//!     to_char(-1,'9PL')   PG  -1     SPG   1-
//!     to_char(1,'S')      PG  <empty>   SPG  +
//!     to_char(1,'PR')     PG  <empty>   SPG  <space>
//!
//! `PL` is a PLUS column, not a sign column: it shows `+` for a
//! non-negative value and a blank for a negative one, and the minus goes to
//! the leading position. It was being treated like `SG`, which really is a
//! sign column. A lone trailing `S` and a lone `PR` print nothing at all,
//! while a lone `SG` and a lone `MI` do print.
//!
//!     single letters   4 differing   (56 when F33 opened)
//!     keywords         4 differing   (13 when F33 opened)
//!
//! All four remaining keywords are a different class: `TH`, `th` and `SS`
//! alone are ERRORS in PG and answers here, and `G` is positional in PG
//! (`to_char(12,'9G9')` is ` 1,2`) where it enables thousands grouping
//! here. `V` alone is the last format shape.
//!
//! Measured against PG18 with `lc_monetary` and `lc_numeric` both set to
//! `C`, which is what SPG reports.

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

/// Each sign element, both signs, with digit slots.
#[test]
fn round631_sign_elements_with_slots() {
    let mut e = Engine::new();
    assert_eq!(vals(&mut e, "SELECT to_char(1,'9MI'), to_char(-1,'9MI')"), vec!["1 |1-"]);
    assert_eq!(
        vals(&mut e, "SELECT to_char(1,'9PL'), to_char(-1,'9PL')"),
        vec![" 1+|-1 "],
        "PL is a plus column: the minus goes to the leading position and PL blanks"
    );
    assert_eq!(vals(&mut e, "SELECT to_char(1,'9SG'), to_char(-1,'9SG')"), vec!["1+|1-"]);
    assert_eq!(vals(&mut e, "SELECT to_char(1,'9S'), to_char(-1,'9S')"), vec!["1+|1-"]);
    assert_eq!(vals(&mut e, "SELECT to_char(1,'S9'), to_char(-1,'S9')"), vec!["+1|-1"]);
    assert_eq!(vals(&mut e, "SELECT to_char(1,'9PR'), to_char(-1,'9PR')"), vec![" 1 |<1>"]);
}

/// …and with none, where they used to vanish.
#[test]
fn round631_sign_elements_without_slots() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT to_char(1,'MI'), to_char(-1,'MI')"),
        vec![" |-"],
        "the sign was lost entirely — the overflow branch returned before it"
    );
    assert_eq!(vals(&mut e, "SELECT to_char(1,'PL'), to_char(-1,'PL')"), vec!["+| "]);
    assert_eq!(vals(&mut e, "SELECT to_char(1,'SG'), to_char(-1,'SG')"), vec!["+|-"]);
    assert_eq!(
        vals(&mut e, "SELECT to_char(1,'S'), to_char(-1,'S')"),
        vec!["|"],
        "a lone trailing S prints nothing, unlike a lone SG"
    );
    assert_eq!(vals(&mut e, "SELECT to_char(1,'PR'), to_char(-1,'PR')"), vec!["|"]);
}

/// The half that made the one-sided fix wrong: no slots means no digits.
#[test]
fn round631_no_numeric_field_prints_no_digits() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT to_char(1,'B'), to_char(1,'C'), to_char(1234,'B')"),
        vec!["||"],
        "round 630's attempt printed the digits here"
    );
    assert_eq!(vals(&mut e, "SELECT to_char(1,'L'), to_char(1,'G')"), vec![" | "]);
    // A decimal separator IS a numeric field, so these still render.
    assert_eq!(vals(&mut e, "SELECT to_char(1,'D'), to_char(12,'DAY')"), vec![" .| .AY"]);
    // And nothing about the ordinary pictures moved.
    assert_eq!(
        vals(&mut e, "SELECT to_char(1234.5,'9999.9'), to_char(-1234.5,'9999.9'), to_char(1234,'9,999')"),
        vec![" 1234.5|-1234.5| 1,234"]
    );
    assert_eq!(vals(&mut e, "SELECT to_char(1234.5,'FM9999.9'), to_char(0,'B9999.99')"), vec!["1234.5|     .00"]);
}
