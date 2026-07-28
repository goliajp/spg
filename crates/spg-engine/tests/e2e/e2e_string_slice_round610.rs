//! v7.39 (round 610) — taking a slice of a string built a vector of the
//! whole string first.
//!
//! Round 608 took the operand COPIES out of the string functions. What was
//! left in these two is the shape underneath: to hand back five characters
//! they collected the entire input into a `Vec<char>` — four bytes for every
//! character of it — indexed that, and collected the answer back out.
//! Counted over 200k rows against a TEXT column:
//!
//!     count(s)                          0 allocations a row    3.3 ms
//!     count(substr(s,2,5))              3.5                   28.9
//!     count(left(s,5))                  5.5                   37.6
//!
//! Both walk byte offsets now and slice once:
//!
//!     left / right         5.5 -> 2 allocations a row   37.6 -> 19.5 ms
//!     substr / substring   3.5 -> 1                     28.9 -> 15.6
//!
//! and over pgwire on 500k rows against PG18:
//!
//!     left(s,5)                102.30 -> 57.08   PG 9.81   10.40x -> 5.82x
//!     right(s,5)               100.32 -> 62.59   PG 9.48   10.38x -> 6.60x
//!     substring(s from 2 for 5) 70.31 -> 36.23   PG 9.79    6.90x -> 3.70x
//!     substr(s,2,5)             75.07 -> 42.47   PG 9.94    7.70x -> 4.27x
//!
//! Characters, not bytes, is the whole contract here, and it is the part a
//! byte walk can get wrong — so the pins carry multi-byte text through every
//! shape: a positive and a negative count on both ends, a count past the
//! length, a start before the string, a zero length, and the empty string.
//! `left` with a positive count is the one case that never needs the total,
//! and it does not compute one.
//!
//! All 16 shapes were run against the previous binary and against this one:
//! SPG's answers are byte-identical. Against live PG18 fifteen match; the
//! sixteenth is `substr(s FROM 2 FOR 2)`, which PG rejects as a syntax error
//! (only `substring` has that spelling there) and SPG answers. That is the
//! parser's leniency and predates this round — the change is confined to two
//! evaluator arms.

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

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE lt (id INT, s TEXT)").unwrap();
    e.execute(
        "INSERT INTO lt VALUES (1,'abcdef'),(2,''),(3,NULL),(4,'日本語テキスト'),(5,'ábç'),(6,'x')",
    )
    .unwrap();
    e
}

/// `left` / `right`, including the negative counts that mean "drop from the
/// other end" and the counts that run past the string.
#[test]
fn round610_left_and_right() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT id, left(s,3), left(s,0), left(s,99), left(s,-2) FROM lt ORDER BY id"),
        vec![
            "1|abc||abcdef|abcd",
            "2||||",
            "3|NULL|NULL|NULL|NULL",
            "4|日本語||日本語テキスト|日本語テキ",
            "5|ábç||ábç|á",
            "6|x||x|",
        ],
        "left(s,-2) drops the last two CHARACTERS"
    );
    assert_eq!(
        vals(&mut e, "SELECT id, right(s,3), right(s,0), right(s,99), right(s,-2) FROM lt ORDER BY id"),
        vec![
            "1|def||abcdef|cdef",
            "2||||",
            "3|NULL|NULL|NULL|NULL",
            "4|キスト||日本語テキスト|語テキスト",
            "5|ábç||ábç|ç",
            "6|x||x|",
        ],
        "right(s,-2) drops the first two"
    );
    assert_eq!(
        vals(&mut e, "SELECT id, left(s,1), right(s,1), left(s,-99), right(s,-99) FROM lt ORDER BY id"),
        vec![
            "1|a|f||",
            "2||||",
            "3|NULL|NULL|NULL|NULL",
            "4|日|ト||",
            "5|á|ç||",
            "6|x|x||",
        ],
        "a drop bigger than the string leaves nothing"
    );
    assert_eq!(
        vals(&mut e, "SELECT id, length(left(s,3)), length(right(s,3)) FROM lt ORDER BY id"),
        vec!["1|3|3", "2|0|0", "3|NULL|NULL", "4|3|3", "5|3|3", "6|1|1"],
        "three characters is three whatever their byte width"
    );
    assert_eq!(
        vals(&mut e, "SELECT left('日本語テキスト',2), right('日本語テキスト',2), left('ábç',2), right('ábç',2)"),
        vec!["日本|スト|áb|bç"]
    );
    assert_eq!(
        vals(&mut e, "SELECT left('',1), right('',1), left(123::TEXT,2), right(456::TEXT,2)"),
        vec!["||12|56"]
    );
}

/// `substring` / `substr`, whose start is 1-based and may be zero or
/// negative.
#[test]
fn round610_substring() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, substring(s from 2 for 3), substring(s from 1 for 1), substring(s from 2) \
             FROM lt ORDER BY id"
        ),
        vec![
            "1|bcd|a|bcdef",
            "2|||",
            "3|NULL|NULL|NULL",
            "4|本語テ|日|本語テキスト",
            "5|bç|á|bç",
            "6||x|",
        ],
        "no FOR takes the rest"
    );
    assert_eq!(
        vals(&mut e, "SELECT id, substr(s,2,3), substr(s,1,0), substr(s,99,3), substr(s,0,3) FROM lt ORDER BY id"),
        vec![
            "1|bcd|||ab",
            "2||||",
            "3|NULL|NULL|NULL|NULL",
            "4|本語テ|||日本",
            "5|bç|||áb",
            "6||||x",
        ],
        "start 0 spends one of the three on the position before the string"
    );
    assert_eq!(
        vals(&mut e, "SELECT id, substr(s,-1,3), substr(s,-5,3), substring(s from 0 for 3) FROM lt ORDER BY id"),
        vec![
            "1|a||ab",
            "2|||",
            "3|NULL|NULL|NULL",
            "4|日||日本",
            "5|á||áb",
            "6|x||x",
        ],
        "a negative start counts toward the string and the length is spent getting there"
    );
    assert_eq!(
        vals(&mut e, "SELECT id, substring(s from 2 for 99), substr(s,2,0), substr(s,3) FROM lt ORDER BY id"),
        vec![
            "1|bcdef||cdef",
            "2|||",
            "3|NULL|NULL|NULL",
            "4|本語テキスト||語テキスト",
            "5|bç||ç",
            "6|||",
        ]
    );
    assert_eq!(
        vals(&mut e, "SELECT id, length(substr(s,2,3)), length(substring(s from 2)) FROM lt ORDER BY id"),
        vec!["1|3|5", "2|0|0", "3|NULL|NULL", "4|3|6", "5|2|2", "6|0|0"]
    );
    assert_eq!(
        vals(&mut e, "SELECT substr('日本語テキスト',2,3), substr('ábç',2,1), substring('ábç' from 3)"),
        vec!["本語テ|b|ç"]
    );
    assert_eq!(
        vals(&mut e, "SELECT substr('',1,1), substring('' from 1 for 1), substr(789::TEXT,2,1)"),
        vec!["||8"]
    );
}

/// The slices feeding the rest of a query.
#[test]
fn round610_slices_in_use() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT id, left(s,3)||'|'||right(s,3) FROM lt ORDER BY id"),
        vec!["1|abc|def", "2||", "3|NULL", "4|日本語|キスト", "5|ábç|ábç", "6|x|x"]
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM lt WHERE left(s,1) = 'a' OR right(s,1) = 'x' ORDER BY id"),
        vec!["1", "6"]
    );
}

/// At the size where building the vector was the cost.
#[test]
fn round610_scale() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT, s TEXT)").unwrap();
    e.execute("INSERT INTO big SELECT gg, 'row' || gg FROM generate_series(1, 20000) gg")
        .unwrap();
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM big WHERE left(s,3) = 'row'"),
        vec!["20000"]
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM big WHERE substr(s,4) = id::TEXT"),
        vec!["20000"],
        "the tail after 'row' is the id on every row"
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM big WHERE right(s, length(id::TEXT)) = id::TEXT"),
        vec!["20000"]
    );
    assert_eq!(
        vals(&mut e, "SELECT count(DISTINCT left(s,4)) FROM big"),
        vals(&mut e, "SELECT count(DISTINCT substring(s from 1 for 4)) FROM big"),
        "the two spellings agree"
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM big WHERE left(s,-3) || right(s,3) = s"),
        vec!["20000"],
        "the negative-count halves put the string back together"
    );
}
