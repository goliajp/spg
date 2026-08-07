//! v7.39 (round 605) — an expression that cannot depend on the row was
//! evaluated for every row.
//!
//! Round 597 folded ANY/ALL's right-hand array once; round 604 measured the
//! same gap still open everywhere else. A counting allocator, 50k rows,
//! against 1 allocation a row for a plain column:
//!
//!     SELECT ('{"a":1}')::JSONB FROM j      10 allocations a row
//!     SELECT ('abc' || 'def')  FROM j        6
//!     SELECT upper('abc')      FROM j        5
//!     WHERE id < ('500')::INT                2   (against 0 for `< 500`)
//!
//! all producing the same value fifty thousand times. Both surfaces fold now
//! — the compiled predicate program emits the value as a literal step, and a
//! constant projection item is evaluated once and cloned:
//!
//!     ('{"a":1}')::JSONB    10 -> 2 allocations   27.59 -> 17.97 ms   PG 6.07
//!     'abc' || 'def'         6 -> 2               23.33 -> 18.12      PG 5.11
//!     id < ('50000')::INT    2 -> 0               10.50 ->  2.24      PG 4.87
//!
//! The predicate shape now costs exactly what `id < 50000` costs (2.18 ms)
//! and beats PG. `upper('abc')` does NOT fold and is unchanged at 22.25:
//! "constant" is round 597's allowlist of node kinds, which excludes
//! function calls because SPG cannot look up a function's volatility, and
//! folding `random()` once would be a wrong answer rather than a slow one.
//!
//! Two things keep this from changing behaviour. An expression that fails to
//! evaluate is left alone, so its error still comes from the row loop in the
//! interpreter's wording and at the same time — including the case where
//! there are no rows and it therefore never fires. And the fold happens per
//! execution with that statement's context, not in a cached plan: every
//! holder of a compiled program builds it during the execution that uses it,
//! which is why `('01/02/2020')::DATE` still answers with the session's
//! `DateStyle` — pinned below by asking the same statement twice under two
//! settings, and matching PG18 under both.
//!
//! All 17 shapes here were checked against live PG18. Fifteen matched; the
//! two that did not are older than this round (verified against the previous
//! binary) and are in the ledger: PG raises a constant's error even when the
//! query matches NO rows, where SPG stays silent, and SPG's parser rejects
//! `RESET TIME ZONE`.

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
    e.execute("CREATE TABLE cf (id INT, s TEXT)").unwrap();
    e.execute("INSERT INTO cf VALUES (1,'a'),(2,'b'),(3,NULL)")
        .unwrap();
    e
}

/// The same value on every row, whatever it was built out of.
#[test]
fn round605_constant_projection_items() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, ('{\"a\":1}')::JSONB, ('500')::INT, ('2020-01-02')::DATE FROM cf ORDER BY id"
        ),
        vec![
            r#"1|{"a": 1}|500|2020-01-02"#,
            r#"2|{"a": 1}|500|2020-01-02"#,
            r#"3|{"a": 1}|500|2020-01-02"#,
        ]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, 2+3*4, 10/4, 'a' = 'b', NOT true FROM cf ORDER BY id"
        ),
        vec![
            "1|14|2|false|false",
            "2|14|2|false|false",
            "3|14|2|false|false"
        ],
        "integer division truncates, and a constant comparison is a constant"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, 'abc' || 'def', ('x')::TEXT FROM cf ORDER BY id"
        ),
        vec!["1|abcdef|x", "2|abcdef|x", "3|abcdef|x"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, NULL::INT, (NULL::INT) + 1, NULL || 'x' FROM cf ORDER BY id"
        ),
        vec!["1|NULL|NULL|NULL", "2|NULL|NULL|NULL", "3|NULL|NULL|NULL",],
        "a constant NULL stays NULL through arithmetic and concatenation"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, ARRAY[1,2,3], ('{1,2}')::INT[] FROM cf ORDER BY id"
        ),
        vec!["1|{1,2,3}|{1,2}", "2|{1,2,3}|{1,2}", "3|{1,2,3}|{1,2}"]
    );
}

/// Folded values still combine with the row's own.
#[test]
fn round605_constants_beside_columns() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, id + (2*3), id::TEXT || ('-' || 'x') FROM cf ORDER BY id"
        ),
        vec!["1|7|1-x", "2|8|2-x", "3|9|3-x"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM cf WHERE id < ('3')::INT ORDER BY id"
        ),
        vec!["1", "2"]
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM cf WHERE ('a') = ('b')"),
        vec!["0"],
        "a constantly-false predicate keeps nothing"
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM cf WHERE (1+1) = 2"),
        vec!["3"],
        "and a constantly-true one keeps everything"
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM cf ORDER BY (2-1), id DESC"),
        vec!["3", "2", "1"],
        "a constant sort key orders nothing, so the second key decides"
    );
    assert_eq!(
        vals(&mut e, "SELECT (1+1) k, count(*) FROM cf GROUP BY 1"),
        vec!["2|3"],
        "and a constant group key is one group"
    );
}

/// A constant that does not evaluate keeps its error where it was — which
/// includes not raising it at all when no row asks.
#[test]
fn round605_failing_constants_keep_their_error() {
    let mut e = seed();
    assert!(
        e.execute("SELECT id, 1/0 FROM cf ORDER BY id").is_err(),
        "division by zero still raises"
    );
    assert!(
        e.execute("SELECT id, ('abc')::INT FROM cf ORDER BY id")
            .is_err(),
        "an impossible cast still raises"
    );
    assert_eq!(
        vals(&mut e, "SELECT id, 1/0 FROM cf WHERE false"),
        Vec::<String>::new(),
        "no rows, no evaluation, no error — unchanged by the fold"
    );
    assert_eq!(
        vals(&mut e, "SELECT id, ('abc')::INT FROM cf WHERE false"),
        Vec::<String>::new()
    );
}

/// What is NOT constant must not be folded: a volatile function, and a
/// value that depends on the session.
#[test]
fn round605_volatile_and_session_dependent_are_not_folded() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(DISTINCT x) FROM (SELECT random() x FROM cf) q"
        ),
        vec!["3"],
        "three rows, three different values — a folded random() would give one"
    );
    // The fold runs per execution with that statement's context, so a
    // session-dependent constant follows the session. `DateStyle` decides
    // both how the literal is READ and how the result is written, so the
    // same statement has to give a different answer after a SET.
    e.execute("SET DateStyle = 'ISO, MDY'").unwrap();
    assert_eq!(
        vals(
            &mut e,
            "SELECT ('2020-01-02')::DATE::TEXT, ('01/02/2020')::DATE::TEXT FROM cf WHERE id = 1"
        ),
        vec!["2020-01-02|2020-01-02"]
    );
    e.execute("SET DateStyle = 'SQL, DMY'").unwrap();
    assert_eq!(
        vals(
            &mut e,
            "SELECT ('2020-01-02')::DATE::TEXT, ('01/02/2020')::DATE::TEXT FROM cf WHERE id = 1"
        ),
        vec!["02/01/2020|01/02/2020"],
        "the same statement, a different DateStyle, a different answer"
    );
    e.execute("SET intervalstyle = 'postgres'").unwrap();
    assert_eq!(
        vals(
            &mut e,
            "SELECT ('1 day 2 hours')::INTERVAL::TEXT FROM cf WHERE id = 1"
        ),
        vec!["1 day 02:00:00"]
    );
    e.execute("SET intervalstyle = 'iso_8601'").unwrap();
    assert_eq!(
        vals(
            &mut e,
            "SELECT ('1 day 2 hours')::INTERVAL::TEXT FROM cf WHERE id = 1"
        ),
        vec!["P1DT2H"]
    );
}

/// At a size where evaluating the constant per row was the cost.
#[test]
fn round605_scale() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT)").unwrap();
    e.execute("INSERT INTO big SELECT gg FROM generate_series(1, 20000) gg")
        .unwrap();
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(DISTINCT x) FROM (SELECT ('abc'||'def') x FROM big) q"
        ),
        vec!["1"]
    );
    assert_eq!(
        vals(&mut e, "SELECT count(*) FROM big WHERE id < ('500')::INT"),
        vals(&mut e, "SELECT count(*) FROM big WHERE id < 500"),
        "the folded predicate keeps the same rows as the literal one"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM big WHERE id = ANY (ARRAY[1,2,3])"
        ),
        vec!["3"]
    );
}
