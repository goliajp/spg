//! v7.39 (round 612) — CONCAT rendered every argument into a string of its
//! own, only to push it into the answer and drop it.
//!
//! Counted over 200k rows against a TEXT column:
//!
//!     count(s)                            0 allocations a row   3.3 ms
//!     count(concat(s, id))                4                    25.3
//!     count(concat_ws('-', s, id))        5                    28.0
//!
//! A text argument already IS the text it renders as, and the separator is
//! rendered once but was owned; the output started at zero capacity and grew
//! into itself. Now the render borrows where it can and the output is sized
//! from the arguments:
//!
//!     concat(s, id)              4 -> 2 allocations a row   25.3 -> 20.8 ms
//!     concat_ws('-', s, id)      5 -> 2                     28.0 -> 19.4
//!     concat_ws('-', s, s, s)          -> 1                          24.0
//!
//! and over pgwire on 500k rows against PG18:
//!
//!     concat(s, id)             79.79 -> 49.26   PG 12.31   5.13x -> 4.00x
//!     concat_ws('-', s, id, g) 108.93 -> 61.81   PG 15.54   5.66x -> 3.98x
//!
//! All 12 shapes here were run against the previous binary as well as this
//! one — SPG's answers are byte-identical — and against live PG18, where
//! eleven match. The twelfth is `concat()` with no arguments at all, which PG
//! rejects ("function concat() does not exist") and SPG answers as the empty
//! string; that is the parser / arity surface and predates this round.
//!
//! What the pins are for is the rules the borrow must not disturb: PG's
//! CONCAT SKIPS a NULL argument rather than being poisoned by it, while a
//! NULL SEPARATOR does poison `concat_ws`; an empty argument is not a NULL
//! and still gets a separator around it; and a non-text argument is rendered
//! through the session's style, which `DateStyle` is used to show.
//!
//! Measured and NOT closed, from the same sweep: `s::VARCHAR` costs 30.6 ms
//! where the identical `s::TEXT` costs 11.2. Probes placed inside the cast
//! split that gap — 12.6 ms to reach the arm and hand the value back, 19.0 to
//! also run the early name checks, 21.7 to run the resolve and the coercion,
//! 30.6 for all of it — so there is no single hotspot: it is the whole
//! per-row re-derivation of a name that is fixed for the statement, and
//! closing it needs the resolution hoisted out of the row loop. Not done.

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
    e.execute("CREATE TABLE ct (id INT, s TEXT, n NUMERIC, d DATE, b BOOLEAN)")
        .unwrap();
    e.execute(
        "INSERT INTO ct VALUES (1,'ab',1.50,'2020-01-02',true),(2,NULL,NULL,NULL,NULL),\
         (3,'日本',3.0,'1999-12-31',false),(4,'',0.25,'2000-02-29',NULL)",
    )
    .unwrap();
    e
}

/// A NULL argument is skipped, not poisonous — the rule the borrow walks past.
#[test]
fn round612_nulls_are_skipped() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, concat(s, id), concat(s, n, d), concat(s) FROM ct ORDER BY id"
        ),
        vec![
            "1|ab1|ab1.502020-01-02|ab",
            "2|2||",
            "3|日本3|日本3.01999-12-31|日本",
            "4|4|0.252000-02-29|",
        ]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, concat_ws('-', s, id), concat_ws('-', s, n, d), concat_ws('-', s) \
             FROM ct ORDER BY id"
        ),
        vec![
            "1|ab-1|ab-1.50-2020-01-02|ab",
            "2|2||",
            "3|日本-3|日本-3.0-1999-12-31|日本",
            "4|-4|-0.25-2000-02-29|",
        ],
        "a skipped NULL takes its separator with it; an EMPTY argument does not"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT concat_ws(NULL, 'a','b') IS NULL, concat_ws('', 'a','b'), concat_ws('--','a',NULL,'b')"
        ),
        vec!["true|ab|a--b"],
        "a NULL SEPARATOR does poison"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT concat_ws('-'), concat(NULL), concat_ws('-', NULL, NULL)"
        ),
        vec!["||"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT concat_ws('-', 'a', '', 'b'), concat('', 'x', '')"
        ),
        vec!["a--b|x"],
        "an empty argument still gets its separators"
    );
}

/// A non-text argument is rendered, and through the session's style.
#[test]
fn round612_rendering() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, concat(b, s), concat_ws('|', b, s) FROM ct ORDER BY id"
        ),
        vec!["1|tab|t|ab", "2||", "3|f日本|f|日本", "4||"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT concat('a', 1, 2.50, true, '2020-01-02'::DATE), concat_ws(',', 'a', 1, 2.50, true)"
        ),
        vec!["a12.50t2020-01-02|a,1,2.50,t"]
    );
    e.execute("SET DateStyle = 'SQL, DMY'").unwrap();
    assert_eq!(
        vals(
            &mut e,
            "SELECT concat('d=', '2020-01-02'::DATE), concat_ws('|','d','2020-01-02'::DATE)"
        ),
        vec!["d=02/01/2020|d|02/01/2020"],
        "the render follows the session, so the borrow must not bypass it"
    );
    e.execute("SET DateStyle = 'ISO, MDY'").unwrap();
    assert_eq!(
        vals(
            &mut e,
            "SELECT concat('日本','語'), concat_ws('・','日','本','語')"
        ),
        vec!["日本語|日・本・語"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, concat_ws(s, 'x', 'y') FROM ct ORDER BY id"
        ),
        vec!["1|xaby", "2|NULL", "3|x日本y", "4|xy"],
        "the separator can be the column, including an empty one"
    );
}

/// Lengths, which is where a capacity estimate would show if it truncated.
#[test]
fn round612_lengths_and_agreement() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, length(concat(s, s, s)), length(concat_ws('..', s, s, s)) FROM ct ORDER BY id"
        ),
        vec!["1|6|10", "2|0|0", "3|6|10", "4|0|4"],
        "counted in characters, so the multi-byte row has to agree"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, concat(s, id) = s || id::TEXT FROM ct WHERE s IS NOT NULL ORDER BY id"
        ),
        vec!["1|true", "3|true", "4|true"],
        "concat agrees with the || it spells, where no argument is NULL"
    );
}

/// At the size where rendering each argument into its own string was the cost.
#[test]
fn round612_scale() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT, s TEXT)").unwrap();
    e.execute("INSERT INTO big SELECT gg, 'row' || gg FROM generate_series(1, 20000) gg")
        .unwrap();
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM big WHERE concat(s, '/', id) = s || '/' || id"
        ),
        vec!["20000"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(DISTINCT concat_ws('|', s, id)) FROM big"
        ),
        vec!["20000"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM big WHERE length(concat_ws('--', s, s)) = 2 * length(s) + 2"
        ),
        vec!["20000"],
        "the separator lands exactly once between two non-NULL arguments"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM big WHERE concat_ws('-', NULL, s, NULL) = s"
        ),
        vec!["20000"],
        "and NULLs leave no separators behind"
    );
}
