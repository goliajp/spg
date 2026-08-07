//! v7.39 (round 608) — a string function copied its operands before reading
//! them.
//!
//! Counted with the allocating probe over 200k rows, against a TEXT column:
//!
//!     count(s)                       0 allocations a row     3.0 ms
//!     count(length(s))               0                       6.7
//!     count(upper(s))                1  (its result)        14.7
//!     count(s || 'x')                5                      25.7
//!     count(replace(s,'row','x'))    4                      30.6
//!     count(split_part(s,'o',2))     4                      30.8
//!     count(position('234' in s))    5.5                    37.1
//!     count(lpad(s,12,'0'))          7.5                    42.3
//!
//! `position` RETURNS AN INTEGER and allocated five and a half times a row.
//! None of it was the answer. `value_to_format_text` renders a value as
//! `String`, which for a `TEXT` operand means copying it, and every one of
//! these reached its operands that way — then `strpos` collected both copies
//! into `Vec<char>` (four bytes a character) to search them, `lpad` collected
//! both into `Vec<char>` and built a padding string before concatenating, and
//! `||` copied both sides and then grew one into the other.
//!
//! There is now a borrowing form of that render — `Cow::Borrowed` when the
//! value already IS the text it renders as — and the functions build only
//! their result:
//!
//!     position / strpos    5.5 -> 0 allocations a row   37.1 -> 18.8 ms
//!     replace                4 -> 1                     30.6 -> 23.3
//!     lpad / rpad          7.5 -> 2                     42.3 -> 25.6
//!     split_part             4 -> 2                     30.8 -> 28.8
//!     s || 'x'               5 -> 3                     25.7 -> 20.7
//!
//! and over pgwire on 500k rows against PG18:
//!
//!     position('234' in s)   101.54 -> 53.55    PG  8.51   12.02x -> 6.29x
//!     strpos(s,'234')        101.83 -> 50.27    PG  8.63   12.08x -> 5.82x
//!     lpad(s,12,'0')         122.52 -> 65.89    PG 14.50    8.43x -> 4.54x
//!     replace(s,'row','x')    83.95 -> 62.73    PG 11.82    7.05x -> 5.31x
//!     split_part(s,'o',2)     81.79 -> 69.87    PG  8.98    9.07x -> 7.78x
//!     s || 'x'                65.56 -> 46.75    PG  7.37    8.74x -> 6.34x
//!
//! `strpos` is the one whose ANSWER had to be re-derived rather than just
//! its allocation: it searches bytes now and converts the hit's byte offset
//! to a character offset, where before it compared two char vectors. PG
//! reports a character position, so multi-byte text is the case that decides
//! whether the two agree — pinned below in both directions (a multi-byte
//! needle in a multi-byte haystack, and an ASCII needle after multi-byte
//! text). `lpad` likewise counts characters rather than collecting them, and
//! cycles the fill instead of indexing a vector of it.
//!
//! All 20 shapes here were checked against live PG18 and matched byte for
//! byte — empty strings, NULLs, an empty needle, an empty fill, a fill
//! longer than one character, a multi-byte fill, negative field positions,
//! and truncation when the target is shorter than the input.

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
    e.execute("CREATE TABLE st (id INT, s TEXT)").unwrap();
    e.execute(
        "INSERT INTO st VALUES (1,'row1'),(2,''),(3,NULL),(4,'日本語テキスト'),\
         (5,'aXbXc'),(6,'  pad  '),(7,'ábç'),(8,'xxx')",
    )
    .unwrap();
    e
}

/// `strpos` reports a CHARACTER position. It searches bytes now, so the
/// multi-byte cases are the ones that decide whether it still does.
#[test]
fn round608_strpos_reports_character_positions() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, strpos(s,'o'), position('o' in s), strpos(s,''), strpos(s,'zzz') \
             FROM st ORDER BY id"
        ),
        vec![
            "1|2|2|1|0",
            "2|0|0|1|0",
            "3|NULL|NULL|NULL|NULL",
            "4|0|0|1|0",
            "5|0|0|1|0",
            "6|0|0|1|0",
            "7|0|0|1|0",
            "8|0|0|1|0",
        ],
        "an empty needle is found at 1; a missing one gives 0; NULL poisons"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, strpos(s,'本'), position('テ' in s), strpos(s,'ç') FROM st ORDER BY id"
        ),
        vec![
            "1|0|0|0",
            "2|0|0|0",
            "3|NULL|NULL|NULL",
            "4|2|4|0",
            "5|0|0|0",
            "6|0|0|0",
            "7|0|0|3",
            "8|0|0|0",
        ],
        "'本' is the 2nd CHARACTER of 日本語テキスト though the 4th byte, \
         'テ' the 4th, and 'ç' the 3rd of ábç"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT strpos('aaa','aa'), strpos('','x'), strpos('',''), strpos('x','')"
        ),
        vec!["1|0|1|1"],
        "leftmost match, and the empty-needle rule at both ends"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, strpos('abcabc', s), position(s in 'abcabc') FROM st ORDER BY id"
        ),
        vec![
            "1|0|0",
            "2|1|1",
            "3|NULL|NULL",
            "4|0|0",
            "5|0|0",
            "6|0|0",
            "7|0|0",
            "8|0|0",
        ],
        "the operand can be the needle as easily as the haystack"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT strpos(123::TEXT,'2'), replace(456::TEXT,'5','x'), lpad(7::TEXT,3,'0')"
        ),
        vec!["2|4x6|007"],
        "a non-text operand still renders through the owned path"
    );
}

/// `lpad` / `rpad` count characters and cycle the fill.
#[test]
fn round608_padding_counts_characters() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, lpad(s,8,'0'), rpad(s,8,'0'), lpad(s,2,'0'), rpad(s,2,'0') \
             FROM st ORDER BY id"
        ),
        vec![
            "1|0000row1|row10000|ro|ro",
            "2|00000000|00000000|00|00",
            "3|NULL|NULL|NULL|NULL",
            "4|0日本語テキスト|日本語テキスト0|日本|日本",
            "5|000aXbXc|aXbXc000|aX|aX",
            "6|0  pad  |  pad  0|  |  ",
            "7|00000ábç|ábç00000|áb|áb",
            "8|00000xxx|xxx00000|xx|xx",
        ],
        "a too-long input truncates from the LEFT for both, by characters"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, lpad(s,10,'ab'), rpad(s,10,'ab'), lpad(s,10,''), rpad(s,10,'') \
             FROM st ORDER BY id"
        ),
        vec![
            "1|abababrow1|row1ababab|row1|row1",
            "2|ababababab|ababababab||",
            "3|NULL|NULL|NULL|NULL",
            "4|aba日本語テキスト|日本語テキストaba|日本語テキスト|日本語テキスト",
            "5|ababaaXbXc|aXbXcababa|aXbXc|aXbXc",
            "6|aba  pad  |  pad  aba|  pad  |  pad  ",
            "7|abababaábç|ábçabababa|ábç|ábç",
            "8|abababaxxx|xxxabababa|xxx|xxx",
        ],
        "a multi-character fill cycles; an EMPTY fill leaves the input alone"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, lpad(s,10,'日本'), rpad(s,10,'日本') FROM st ORDER BY id"
        ),
        vec![
            "1|日本日本日本row1|row1日本日本日本",
            "2|日本日本日本日本日本|日本日本日本日本日本",
            "3|NULL|NULL",
            "4|日本日日本語テキスト|日本語テキスト日本日",
            "5|日本日本日aXbXc|aXbXc日本日本日",
            "6|日本日  pad  |  pad  日本日",
            "7|日本日本日本日ábç|ábç日本日本日本日",
            "8|日本日本日本日xxx|xxx日本日本日本日",
        ],
        "the cycle counts characters of the fill too, and can stop mid-cycle"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, lpad(s,8), rpad(s,8), lpad(s,0,'x'), rpad(s,-1,'x') FROM st WHERE id IN (1,4)"
        ),
        vec![
            "1|    row1|row1    ||",
            "4| 日本語テキスト|日本語テキスト ||"
        ],
        "the fill defaults to a space; a non-positive target is the empty string"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT lpad('',5,'z'), rpad('',5,'z'), lpad('abc',3,'z'), rpad('abc',3,'z')"
        ),
        vec!["zzzzz|zzzzz|abc|abc"]
    );
    assert_eq!(
        vals(&mut e, "SELECT lpad('ábç',5,'ñ'), rpad('ábç',5,'ñ')"),
        vec!["ññábç|ábçññ"]
    );
}

/// `replace` and `split_part`, whose operands are now all borrowed.
#[test]
fn round608_replace_and_split_part() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, replace(s,'o','0'), replace(s,'','X'), replace(s,'X','') FROM st ORDER BY id"
        ),
        vec![
            "1|r0w1|row1|row1",
            "2|||",
            "3|NULL|NULL|NULL",
            "4|日本語テキスト|日本語テキスト|日本語テキスト",
            "5|aXbXc|aXbXc|abc",
            "6|  pad  |  pad  |  pad  ",
            "7|ábç|ábç|ábç",
            "8|xxx|xxx|xxx",
        ],
        "an empty `from` is a no-op; an empty `to` deletes"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, replace(s,'日','X'), replace(s,'ábç','Y') FROM st ORDER BY id"
        ),
        vec![
            "1|row1|row1",
            "2||",
            "3|NULL|NULL",
            "4|X本語テキスト|日本語テキスト",
            "5|aXbXc|aXbXc",
            "6|  pad  |  pad  ",
            "7|ábç|Y",
            "8|xxx|xxx",
        ]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT replace('aaa','aa','b'), replace('abab','ab','ba')"
        ),
        vec!["ba|baba"],
        "non-overlapping, left to right, with no re-scan of what was inserted"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, split_part(s,'X',1), split_part(s,'X',2), split_part(s,'X',-1) \
             FROM st ORDER BY id"
        ),
        vec![
            "1|row1||row1",
            "2|||",
            "3|NULL|NULL|NULL",
            "4|日本語テキスト||日本語テキスト",
            "5|a|b|c",
            "6|  pad  ||  pad  ",
            "7|ábç||ábç",
            "8|xxx||xxx",
        ],
        "a negative field counts from the end"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, split_part(s,'',1), split_part(s,'',-1), split_part(s,'o',9) \
             FROM st WHERE id IN (1,5)"
        ),
        vec!["1|row1|row1|", "5|aXbXc|aXbXc|"],
        "an empty delimiter makes the whole string field 1 and -1; past the end is ''"
    );
    assert_eq!(vals(&mut e, "SELECT split_part('a,b,c',',',3)"), vec!["c"]);
}

/// `||`, which now borrows what is already text.
#[test]
fn round608_concat_borrows() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, s || 'x', 'y' || s, s || s, s || NULL FROM st ORDER BY id"
        ),
        vec![
            "1|row1x|yrow1|row1row1|NULL",
            "2|x|y||NULL",
            "3|NULL|NULL|NULL|NULL",
            "4|日本語テキストx|y日本語テキスト|日本語テキスト日本語テキスト|NULL",
            "5|aXbXcx|yaXbXc|aXbXcaXbXc|NULL",
            "6|  pad  x|y  pad  |  pad    pad  |NULL",
            "7|ábçx|yábç|ábçábç|NULL",
            "8|xxxx|yxxx|xxxxxx|NULL",
        ],
        "a NULL operand still poisons"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, s || 5, 5 || s, s || 1.50, s || true FROM st WHERE id IN (1,4)"
        ),
        vec![
            "1|row15|5row1|row11.50|row1true",
            "4|日本語テキスト5|5日本語テキスト|日本語テキスト1.50|日本語テキストtrue"
        ],
        "a non-text operand renders through the owned path on either side"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, length(lpad(s,8,'0')), length(rpad(s,8,'0')), length(s||'x') \
             FROM st ORDER BY id"
        ),
        vec![
            "1|8|8|5",
            "2|8|8|1",
            "3|NULL|NULL|NULL",
            "4|8|8|8",
            "5|8|8|6",
            "6|8|8|8",
            "7|8|8|4",
            "8|8|8|4",
        ],
        "lengths are in characters, so the multi-byte rows have to agree too"
    );
}

/// At the size where copying the operands was the cost.
#[test]
fn round608_scale() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT, s TEXT)").unwrap();
    e.execute("INSERT INTO big SELECT gg, 'row' || gg FROM generate_series(1, 20000) gg")
        .unwrap();
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM big WHERE strpos(s, '234') > 0"
        ),
        vec!["40"],
        "checked against live PG18, which answers 40 for the same table"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM big WHERE strpos(s,'234') = position('234' in s)"
        ),
        vec!["20000"],
        "the two spellings agree on every row"
    );
    assert_eq!(
        vals(&mut e, "SELECT count(DISTINCT lpad(s, 12, '0')) FROM big"),
        vec!["20000"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM big WHERE length(lpad(s,12,'0')) = 12"
        ),
        vec!["20000"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM big WHERE replace(s,'row','') = id::TEXT"
        ),
        vec!["20000"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM big WHERE split_part(s || 'X' || id, 'X', 2) = id::TEXT"
        ),
        vec!["20000"]
    );
}
