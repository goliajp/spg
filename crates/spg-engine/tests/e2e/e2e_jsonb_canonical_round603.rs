//! v7.39 (round 603) — a jsonb scalar was formatted into text, parsed back,
//! and serialised again.
//!
//! JSON is stored as text here, and `to_jsonb` reached its canonical form by
//! writing the value out and handing that text to `canonicalize_value`,
//! which PARSES it and re-serialises. A counting allocator, on 50k rows:
//!
//!     plain projection                     1 allocation a row
//!     to_jsonb(id)                        10
//!     jsonb_build_object('a', id)         18
//!     …the same, then ->> and a cast      26
//!
//! An integer's canonical jsonb is its decimal spelling; a bool's and NULL's
//! are their keywords. Those need no round trip and no `JsonValue` at all.
//! `jsonb_build_object` built `json_build_object`'s spacing (`"a" : 1`) and
//! re-parsed it to get jsonb's (`"a": 1`); when every argument is a simple
//! scalar it now builds the value directly and serialises it once. The
//! ordering, the last-wins duplicate rule and the number canonicalisation
//! still come from `write_json_canonical`, so this changes WHEN the parse
//! happens and nothing about what it produces. Anything richer — NUMERIC,
//! dates, arrays, composites, an already-JSON argument — keeps the old path.
//!
//!     to_jsonb(id)                        10 -> 4 allocations   12.6 -> 5.5 ms
//!     jsonb_build_object('a', id)         18 -> 13              20.7 -> 14.3
//!
//! and over pgwire on 50k rows against PG18:
//!
//!     to_jsonb(id)                        30.78 -> 22.69 ms   PG 7.63
//!     jsonb_build_object('a', id)         38.90 -> 32.51      PG 6.75
//!     …with ->> and a cast                51.58 -> 46.41      PG 11.32
//!
//! The differential written to check it found a wrong answer that predates
//! this round: PG's `to_json` / `to_jsonb` / `row_to_json` / `row_to_jsonb`
//! are STRICT — a NULL argument gives a NULL result — and SPG answered the
//! JSON scalar `null` for all of them. An e2e test pinned that behaviour,
//! with a comment asserting PG did the same. Asked live, PG18 says:
//!
//!     SELECT to_json(NULL::int) IS NULL     →  t
//!     SELECT to_jsonb('null'::json)::TEXT   →  null
//!     SELECT jsonb_build_object('a', NULL)  →  {"a": null}
//!
//! so the functions are strict, a JSON `null` VALUE is a different thing,
//! and a NULL inside a builder is still the JSON `null`. All three now hold.
//!
//! All 16 shapes here were checked against live PG18 and matched.

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
    e.execute(
        "CREATE TABLE jb (id INT, b BIGINT, f DOUBLE PRECISION, n NUMERIC(10,3), \
         s TEXT, bo BOOLEAN, d DATE, a INT[])",
    )
    .unwrap();
    e.execute(
        "INSERT INTO jb VALUES \
         (1, 9007199254740993, 1.5, 1.500, 'x', true, '2020-01-02', ARRAY[1,2]),\
         (2, -1, -0.0, 0.000, 'quote\"and\\backslash', false, '1999-12-31', ARRAY[]::INT[]),\
         (3, 0, 1e10, 12345.678, '', NULL, NULL, NULL),\
         (4, NULL, NULL, NULL, NULL, NULL, NULL, NULL)",
    )
    .unwrap();
    e
}

/// The scalars that skip the round trip have to produce what the round trip
/// produced — including a 64-bit integer that a float would round.
#[test]
fn round603_scalar_canonical_forms() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, to_jsonb(id), to_jsonb(b), to_jsonb(bo), to_jsonb(s) FROM jb ORDER BY id"
        ),
        vec![
            r#"1|1|9007199254740993|true|"x""#,
            r#"2|2|-1|false|"quote\"and\\backslash""#,
            r#"3|3|0|NULL|"""#,
            "4|4|NULL|NULL|NULL",
        ],
        "escaping, and a bigint past 2^53"
    );
    assert_eq!(
        vals(&mut e, "SELECT to_jsonb(9007199254740993::BIGINT) FROM jb WHERE id = 1"),
        vec!["9007199254740993"],
        "no float rounding on the way through"
    );
}

/// The kinds that are NOT simple keep the old path and must be unchanged.
#[test]
fn round603_rich_values_keep_the_round_trip() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, to_jsonb(f), to_jsonb(n), to_jsonb(d), to_jsonb(a) FROM jb ORDER BY id"
        ),
        vec![
            r#"1|1.5|1.500|"2020-01-02"|[1, 2]"#,
            r#"2|0|0.000|"1999-12-31"|[]"#,
            "3|10000000000|12345.678|NULL|NULL",
            "4|NULL|NULL|NULL|NULL",
        ],
        "NUMERIC keeps its scale, a float exponent is expanded, arrays and dates unchanged"
    );
    assert_eq!(
        vals(&mut e, "SELECT id, to_json(id), to_json(s), to_json(n) FROM jb ORDER BY id"),
        vec![
            "1|1|\"x\"|1.500",
            r#"2|2|"quote\"and\\backslash"|0.000"#,
            "3|3|\"\"|12345.678",
            "4|4|NULL|NULL",
        ],
        "to_json is not canonicalised and is not affected"
    );
}

/// The object builder: ordering, duplicates, escaping, and the spellings
/// that fall back.
#[test]
fn round603_build_object_canonical() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT id, jsonb_build_object('a', id, 'b', s) FROM jb ORDER BY id"),
        vec![
            r#"1|{"a": 1, "b": "x"}"#,
            r#"2|{"a": 2, "b": "quote\"and\\backslash"}"#,
            r#"3|{"a": 3, "b": ""}"#,
            r#"4|{"a": 4, "b": null}"#,
        ]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT jsonb_build_object('bbb', 1, 'a', 2, 'cc', 3, 'b', 4) FROM jb WHERE id = 1"
        ),
        vec![r#"{"a": 2, "b": 4, "cc": 3, "bbb": 1}"#],
        "keys sort by length then bytes"
    );
    assert_eq!(
        vals(&mut e, "SELECT jsonb_build_object('k', 1, 'k', 2, 'k', 3) FROM jb WHERE id = 1"),
        vec![r#"{"k": 3}"#],
        "duplicate keys are last-wins"
    );
    assert_eq!(
        vals(
            &mut e,
            r#"SELECT jsonb_build_object('a"b', 'c\d', 'tab', E'x\ty') FROM jb WHERE id = 1"#
        ),
        vec![r#"{"a\"b": "c\\d", "tab": "x\ty"}"#],
        "escaping in keys and values"
    );
    assert_eq!(
        vals(&mut e, "SELECT jsonb_build_object(7, 'seven', 10, 'ten') FROM jb WHERE id = 1"),
        vec![r#"{"7": "seven", "10": "ten"}"#],
        "a numeric key becomes its decimal string, and then sorts as one"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, jsonb_build_object('n', n, 'd', d, 'arr', a) FROM jb ORDER BY id"
        ),
        vec![
            r#"1|{"d": "2020-01-02", "n": 1.500, "arr": [1, 2]}"#,
            r#"2|{"d": "1999-12-31", "n": 0.000, "arr": []}"#,
            r#"3|{"d": null, "n": 12345.678, "arr": null}"#,
            r#"4|{"d": null, "n": null, "arr": null}"#,
        ],
        "a rich value sends the whole object down the old path"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT jsonb_build_object('o', jsonb_build_object('i', 1), 'a', 2) FROM jb WHERE id = 1"
        ),
        vec![r#"{"a": 2, "o": {"i": 1}}"#],
        "a nested jsonb argument is already JSON, so it falls back too"
    );
    assert_eq!(
        vals(&mut e, "SELECT json_build_object('a', 1, 'b', 2) FROM jb WHERE id = 1"),
        vec![r#"{"a" : 1, "b" : 2}"#],
        "json_build_object keeps its own spacing and is untouched"
    );
}

/// The strictness the differential caught, and that the value round-trips.
#[test]
fn round603_null_argument_is_null() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT to_jsonb(NULL::INT) IS NULL, to_json(NULL::TEXT) IS NULL"),
        vec!["true|true"]
    );
    assert_eq!(
        vals(&mut e, "SELECT to_jsonb('null'::JSON)"),
        vec!["null"],
        "a JSON null VALUE is not a NULL argument"
    );
    assert_eq!(
        vals(&mut e, "SELECT id, jsonb_build_object('v', b, 'f', bo) FROM jb ORDER BY id"),
        vec![
            r#"1|{"f": true, "v": 9007199254740993}"#,
            r#"2|{"f": false, "v": -1}"#,
            r#"3|{"f": null, "v": 0}"#,
            r#"4|{"f": null, "v": null}"#,
        ],
        "a NULL inside a builder is still the JSON null"
    );
}

/// What the built value is FOR: reading it back, comparing it, aggregating
/// it. A canonical form that differed would show here.
#[test]
fn round603_built_values_behave_as_jsonb() {
    let mut e = seed();
    assert_eq!(
        vals(&mut e, "SELECT id, (jsonb_build_object('a', id) ->> 'a')::INT FROM jb ORDER BY id"),
        vec!["1|1", "2|2", "3|3", "4|4"]
    );
    assert_eq!(
        vals(
            &mut e,
            r#"SELECT jsonb_build_object('a',1) = '{"a": 1}'::JSONB, jsonb_build_object('a',1) @> '{"a":1}'::JSONB FROM jb WHERE id = 1"#
        ),
        vec!["true|true"],
        "equal to the same object written by hand, and containing it"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*), count(DISTINCT x::TEXT) FROM \
             (SELECT jsonb_build_object('a', id % 2) x FROM jb) q"
        ),
        vec!["4|2"],
        "four rows, two distinct canonical texts"
    );
}
