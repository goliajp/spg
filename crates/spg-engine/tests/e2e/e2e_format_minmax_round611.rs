//! v7.39 (round 611) — picking the smaller of two integers, and filling in a
//! format string, both built their answer out of vectors of copies.
//!
//! Counted over 200k rows, against columns that need no heap at all for the
//! answer:
//!
//!     count(least(id, 0))              4 allocations a row   27.9 ms
//!     count(greatest(id, 0))           4                     22.5
//!     count(format('%s/%s', s, id))   10                     46.3
//!
//! `least(id, 0)` compares two INTEGERS and returns one of them. The four
//! allocations were a `Vec` of references to the non-NULL arguments, a second
//! `Vec` holding an owned clone of every one of them, a `Vec` of their types
//! for the widening, and the clone of the winner. Only the last is the
//! answer. Nothing needs an owned copy unless a text argument has to be
//! coerced to a sibling's type — `greatest(time_col, '14:00')`, which is why
//! the copies were there — so that case keeps the owned pass and everything
//! else compares in place.
//!
//! `format` built ten. The format string was cloned; every conversion spec
//! built two `String`s to hold its digits (one for the `n$` position, one for
//! the width, and the first was cloned into the second); every argument was
//! cloned out of the slice; every one was rendered into another owned
//! `String`; and the output started at zero capacity and reallocated its way
//! up. The digits are numbers now, the arguments and the format string are
//! read in place, a `%s` of a text value borrows it, and the output is sized
//! once.
//!
//!     least / greatest    4 -> 1 allocations a row   27.9 -> 18.0 ms
//!     format('%s/%s')    10 -> 6                     46.3 -> 36.3
//!
//! and over pgwire on 500k rows against PG18:
//!
//!     least(id,0)             84.28 -> 51.38   PG  7.23   10.19x -> 7.11x
//!     greatest(id,0)          71.82 -> 34.98   PG  6.74    7.56x -> 5.19x
//!     format('%s/%s', s, id) 158.75 -> 92.37   PG 19.34    7.19x -> 4.78x
//!
//! All 18 shapes here were checked against live PG18 and matched byte for
//! byte: explicit `n$` argument positions, a `*` width taken from an argument
//! (positive, negative — which left-justifies — and zero), the `-` flag, a
//! width that is shorter than the value (PG pads but never truncates),
//! multi-byte text under a width, `%I` and `%L` quoting including a value
//! that needs doubling, NULL under each specifier, and GREATEST / LEAST over
//! text, numeric, date and time with their type widening.
//!
//! Measured and NOT closed: `format('%s', s)` still costs 5 allocations a row
//! against 1 for `upper(s)`; the remaining ones were not located.

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
    e.execute("CREATE TABLE ft (id INT, s TEXT, n NUMERIC, d DATE)")
        .unwrap();
    e.execute(
        "INSERT INTO ft VALUES (1,'ab',1.50,'2020-01-02'),(2,NULL,NULL,NULL),\
         (3,'日本',3.0,'1999-12-31'),(4,'it''s',0.25,'2000-02-29')",
    )
    .unwrap();
    e
}

/// The conversion specs, whose digits are parsed as numbers now.
#[test]
fn round611_format_specs() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, format('%s/%s', s, id), format('%s', s), format('%s', n) FROM ft ORDER BY id"
        ),
        vec![
            "1|ab/1|ab|1.50",
            "2|/2||",
            "3|日本/3|日本|3.0",
            "4|it's/4|it's|0.25",
        ],
        "a NULL renders as empty under %s"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT format('%2$s-%1$s', 'a', 'b'), format('%1$s%1$s', 'x'), format('%3$s', 'a','b','c')"
        ),
        vec!["b-a|xx|c"],
        "explicit positions, including the same one twice"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT format('[%10s]', 'ab'), format('[%-10s]', 'ab'), format('[%3s]', 'abcdef')"
        ),
        vec!["[        ab]|[ab        ]|[abcdef]"],
        "a width pads but never truncates"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT format('[%*s]', 6, 'ab'), format('[%*s]', -6, 'ab'), format('[%*s]', 0, 'ab')"
        ),
        vec!["[    ab]|[ab    ]|[ab]"],
        "a * width comes from an argument, and a negative one left-justifies"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT format('[%10s]', '日本'), format('[%-10s]', '日本')"
        ),
        vec!["[        日本]|[日本        ]"],
        "the width counts characters"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT format('%L', NULL::TEXT), format('%s', NULL::TEXT), format('%%'), format('a%%b')"
        ),
        vec!["NULL||%|a%b"]
    );
    assert!(
        e.execute("SELECT format('%I', NULL::TEXT)").is_err(),
        "%I refuses a NULL identifier"
    );
}

/// `%I` and `%L`, which quote.
#[test]
fn round611_format_quoting() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, format('%I', s), format('%L', s), format('%L', n) FROM ft WHERE s IS NOT NULL ORDER BY id"
        ),
        vec![
            "1|ab|'ab'|'1.50'",
            "3|\"日本\"|'日本'|'3.0'",
            "4|\"it's\"|'it''s'|'0.25'",
        ]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT format('%I', 'Weird Name'), format('%I', 'plain'), format('%L', 'it''s')"
        ),
        vec!["\"Weird Name\"|plain|'it''s'"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT format('%s', 't'::BOOL), format('%s', '2020-01-02'::DATE), format('%s', 1.50::NUMERIC)"
        ),
        vec!["t|2020-01-02|1.50"],
        "a non-text value renders through the owned path"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, format('%s|%s|%s', s, n, d) FROM ft ORDER BY id"
        ),
        vec![
            "1|ab|1.50|2020-01-02",
            "2|||",
            "3|日本|3.0|1999-12-31",
            "4|it's|0.25|2000-02-29",
        ]
    );
}

/// GREATEST / LEAST: the winner, and the type it takes.
#[test]
fn round611_min_max() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, greatest(id, 2), least(id, 2), greatest(s,'b'), least(s,'b') FROM ft ORDER BY id"
        ),
        vec!["1|2|1|b|ab", "2|2|2|b|b", "3|3|2|日本|b", "4|4|2|it's|b",],
        "a NULL argument is IGNORED, not poisonous"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, greatest(n, 1), least(n, 1), greatest(d,'2010-01-01'), least(d,'2010-01-01') \
             FROM ft ORDER BY id"
        ),
        vec![
            "1|1.50|1|2020-01-02|2010-01-01",
            "2|1|1|2010-01-01|2010-01-01",
            "3|3.0|1|2010-01-01|1999-12-31",
            "4|1|0.25|2010-01-01|2000-02-29",
        ],
        "the untyped date literal takes the column's type — the case the \
         owned pass exists for"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT greatest('12:00'::TIME, '14:00'), least('12:00'::TIME, '14:00')"
        ),
        vec!["14:00:00|12:00:00"],
        "and for TIME, which compared as text before that pass existed"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT greatest(1, 2.5), greatest(1,2.5)/2, least(3, 2.5)/2"
        ),
        vec!["2.5|1.25000000000000000000|1.25000000000000000000"],
        "the winner is widened, so the division is not an integer one"
    );
    assert_eq!(
        vals(&mut e, "SELECT greatest(1::SMALLINT, 2::BIGINT)"),
        vec!["2"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT greatest(NULL, 3, NULL), least(NULL, 3, NULL), greatest(NULL::INT, NULL::INT) IS NULL"
        ),
        vec!["3|3|true"],
        "all-NULL is NULL"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT greatest(1), least(1), greatest('a','b','c'), least('a','b','c')"
        ),
        vec!["1|1|c|a"],
        "one argument, and more than two"
    );
}

/// At the size where the vectors were the cost.
#[test]
fn round611_scale() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT, s TEXT)").unwrap();
    e.execute("INSERT INTO big SELECT gg, 'row' || gg FROM generate_series(1, 20000) gg")
        .unwrap();
    assert_eq!(
        vals(
            &mut e,
            "SELECT sum(least(id, 100)), sum(greatest(id, 19900)) FROM big"
        ),
        vec!["1995050|398005050"],
        "checked against live PG18, which answers the same"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM big WHERE least(id, 100) = 100"
        ),
        vec!["19901"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM big WHERE format('%s/%s', s, id) = s || '/' || id"
        ),
        vec!["20000"],
        "format agrees with the concatenation it spells"
    );
    assert_eq!(
        vals(&mut e, "SELECT count(DISTINCT format('[%8s]', s)) FROM big"),
        vec!["20000"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM big WHERE format('%2$s|%1$s', id, s) = s || '|' || id"
        ),
        vec!["20000"],
        "and so does the explicit-position spelling"
    );
}
