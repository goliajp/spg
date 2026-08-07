//! v7.39 (round 629, F33) — `B` and `C` are picture elements, and a picture
//! with no numeric field gets no sign column.
//!
//! Continuing round 628's scanner. Measured with the oracle's `lc_monetary`
//! set to `C`, which is what SPG reports — round 628 drew the wrong
//! conclusion about `L` by measuring against the oracle's default
//! `en_US.utf8`, so the GUC is part of the measurement now.
//!
//!     single letters   20 differing -> 4     (56 when F33 opened)
//!     keywords         10 differing -> 7     (13 when F33 opened)
//!
//! Two roots closed. `B` and `C` were treated as literals and echoed:
//! `to_char(0,'B9999')` answered `B    0` and `to_char(1234,'C9999')`
//! answered `C 1234`. PG consumes both — `C` is the ISO currency code,
//! empty in the C locale, and `B` blanks the integer digits of a zero.
//! SPG's blanking already agreed with PG; only the echoed letter was wrong.
//!
//! And a picture with no numeric field kept a column reserved for a sign
//! that had no number to sit beside: `to_char(1,'L')` answered two spaces
//! where PG answers one, `to_char(1,'B')` one where PG answers none. A
//! decimal separator counts as a numeric field even with no slots around
//! it — dropping the column without that clause broke `to_char(1,'D')` and
//! `to_char(12,'DAY')`, which the pins below hold.
//!
//! Recorded, measured, not closed: `V` alone leaves a space (it takes its
//! own path); `MI`, `PL` and `SG` alone emit nothing where PG emits ` `,
//! `+`, `+`; `G` is positional in PG (`to_char(12,'9G9')` is ` 1,2`);
//! `TH`, `th` and `SS` alone are errors in PG and answers here.

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

#[test]
fn round629_b_and_c_are_consumed() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT to_char(0,'B9999'), to_char(1,'B9999')"),
        vec!["    0|    1"]
    );
    // B blanks the integer digits of a zero, and did so already — the bug
    // was the letter in front of them.
    assert_eq!(
        vals(&mut e, "SELECT to_char(0,'B9999.99')"),
        vec!["     .00"]
    );
    // C is the ISO currency code: empty in the C locale, either side.
    assert_eq!(
        vals(
            &mut e,
            "SELECT to_char(1234,'C9999'), to_char(1234,'9999C')"
        ),
        vec![" 1234| 1234"]
    );
    assert_eq!(
        vals(&mut e, "SELECT to_char(1,'BB9'), to_char(1,'CC9')"),
        vec![" 1| 1"]
    );
}

#[test]
fn round629_no_numeric_field_means_no_sign_column() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT to_char(1,'B'), to_char(1,'C')"),
        vec!["|"],
        "PG answers nothing for either"
    );
    assert_eq!(
        vals(&mut e, "SELECT to_char(1,'L'), to_char(1,'G')"),
        vec![" | "],
        "one space each — the element's own output, with no sign column"
    );
    // A decimal separator IS a numeric field, so the column stays.
    assert_eq!(vals(&mut e, "SELECT to_char(1,'D')"), vec![" ."]);
    assert_eq!(vals(&mut e, "SELECT to_char(12,'DAY')"), vec![" .AY"]);
    // And a picture with slots is untouched.
    assert_eq!(
        vals(
            &mut e,
            "SELECT to_char(1,'9'), to_char(1,'99'), to_char(1234.5,'9999.9')"
        ),
        vec![" 1|  1| 1234.5"]
    );
    assert_eq!(
        vals(&mut e, "SELECT to_char(-1,'9'), to_char(-1234.5,'9999.9')"),
        vec!["-1|-1234.5"]
    );
}
