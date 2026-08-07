//! v7.39 (round 619) — canonicalising jsonb rebuilt every number and every
//! object's key list.
//!
//! Phase A decomposed `('{"a":' || id || '}')::JSONB` over 200k rows before
//! changing anything, and the answer was not what the shape suggests:
//!
//!     count(s)                          0 allocations a row    2.6 ms
//!     '{"a":' || id || '}'              5.00                  33.0
//!     … ::JSON   (validate only)        5.00                  37.5
//!     … ::JSONB  (canonicalise)        14.00                  75.8
//!
//! Half the cost is building the string, which has nothing to do with JSON.
//! What jsonb adds over json is the canonicalisation — parse into an owned
//! tree, then re-serialise — and that was nine allocations a row.
//!
//! Three of them go here, and none is a rewrite of the semantics:
//!
//!   * a plain integer lexeme IS already the canonical form, so it is handed
//!     back borrowed instead of rebuilt through the full number path. The
//!     full path still decides every other shape, and `eval::json`'s own
//!     unit test asserts the two agree over a generated set of lexemes —
//!     signs, leading zeros, `-0`, trailing zeros, fractions, exponents in
//!     both cases and both signs, and mantissas past i128;
//!   * an object with one entry needs neither the dedup nor the sort, and
//!     the `Vec` those need was built for every object on every row. The
//!     sorting writer is split out and the same unit test asserts the
//!     shortcut spells exactly what it spells;
//!   * the output string is sized from the source rather than grown from
//!     zero.
//!
//!     ('{"a":'||id||'}')::JSONB   14 -> 9 allocations a row   75.8 -> 55.6 ms
//!     jsonb_build_object('a',id)  12 -> 8                     55.5 -> 36.0
//!
//! and over pgwire on 500k rows against PG18:
//!
//!     ('{"a":'||id||'}')::JSONB   192.91 -> 137.48   PG 33.40  5.88x -> 4.12x
//!     jsonb_build_object('a', id) 137.43 ->  93.28   PG 36.27  3.71x -> 2.57x
//!
//! All 13 shapes here were checked against live PG18 and matched byte for
//! byte, and against the previous binary: identical. The canonical form is
//! exactly what it was — keys sorted by length then bytes, duplicates
//! last-wins, `1.0` and `1.50` keeping their scale, `-0` becoming `0`,
//! exponents expanded, strings re-escaped.
//!
//! Recorded, not fixed, and older than this round: SPG's JSON parser accepts
//! two lexemes PG rejects — `00` and `1.` — answering `0` and `1` where PG
//! raises "Token is invalid".
//!
//! Measured and NOT closed: five of the nine allocations are the string
//! concatenation feeding the cast, not the cast; and jsonb is still stored
//! as text, so the parse itself remains.

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

/// Key order, duplicates, and the empty containers.
#[test]
fn round619_object_canonical_form() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            r#"SELECT '{"a":1}'::JSONB, '{"b":2,"a":1}'::JSONB, '{"aa":1,"b":2}'::JSONB"#
        ),
        vec![r#"{"a": 1}|{"a": 1, "b": 2}|{"b": 2, "aa": 1}"#],
        "keys sort by LENGTH first, then bytes — the one-entry shortcut must \
         not change that for its neighbours"
    );
    assert_eq!(
        vals(
            &mut e,
            r#"SELECT '{"k":1,"k":2,"k":3}'::JSONB, '{}'::JSONB, '[]'::JSONB"#
        ),
        vec![r#"{"k": 3}|{}|[]"#],
        "duplicate keys are last-wins, and an empty object takes the shortcut"
    );
    assert_eq!(
        vals(
            &mut e,
            r#"SELECT '[1,2,{"b":1,"a":2}]'::JSONB, '{"x":[1,{"z":1,"y":2}]}'::JSONB"#
        ),
        vec![r#"[1, 2, {"a": 2, "b": 1}]|{"x": [1, {"y": 2, "z": 1}]}"#],
        "nested, where the one-entry object contains a sorting one"
    );
    assert_eq!(
        vals(
            &mut e,
            r#"SELECT '"a\"b"'::JSONB, '"c\\d"'::JSONB, '"日本"'::JSONB, '""'::JSONB"#
        ),
        vec![r#""a\"b"|"c\\d"|"日本"|"""#],
        "strings are re-escaped, not passed through"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT 'true'::JSONB, 'false'::JSONB, 'null'::JSONB"
        ),
        vec!["true|false|null"]
    );
}

/// The numbers, which is where the borrowed shortcut lives.
#[test]
fn round619_number_canonical_form() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT '1'::JSONB, '-1'::JSONB, '0'::JSONB, '-0'::JSONB"
        ),
        vec!["1|-1|0|0"],
        "`-0` canonicalises to `0`, so it is NOT a pass-through"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT '1.0'::JSONB, '1.50'::JSONB, '0.5'::JSONB, '1e3'::JSONB, '1E3'::JSONB, '1e-3'::JSONB"
        ),
        vec!["1.0|1.50|0.5|1000|1000|0.001"],
        "a scale is kept and an exponent is expanded — neither takes the shortcut"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT '-0.0'::JSONB, '10.010'::JSONB, '1e+3'::JSONB"
        ),
        vec!["0.0|10.010|1000"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT '9223372036854775807'::JSONB, '170141183460469231731687303715884105727'::JSONB"
        ),
        vec!["9223372036854775807|170141183460469231731687303715884105727"],
        "past i64 and past i128 — the shortcut is textual, so neither drifts"
    );
}

/// The builders and the accessors, which canonicalise through the same code.
#[test]
fn round619_builders_and_accessors() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT to_jsonb(1), to_jsonb(1.50), to_jsonb('x'::TEXT), to_jsonb(true)"
        ),
        vec![r#"1|1.50|"x"|true"#]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT jsonb_build_object('a',1), jsonb_build_object('b',2,'a',1), jsonb_build_object('k',1,'k',2)"
        ),
        vec![r#"{"a": 1}|{"a": 1, "b": 2}|{"k": 2}"#]
    );
    assert_eq!(
        vals(
            &mut e,
            r#"SELECT ('{"a":'||42||'}')::JSONB, ('{"a":'||42||'}')::JSONB ->> 'a'"#
        ),
        vec![r#"{"a": 42}|42"#],
        "the shape Phase A decomposed"
    );
    assert_eq!(
        vals(
            &mut e,
            r#"SELECT '{"a":1}'::JSONB::TEXT, length('{"a":1}'::JSONB::TEXT)"#
        ),
        vec![r#"{"a": 1}|8"#],
        "the canonical text is what the length counts"
    );
    assert_eq!(
        vals(
            &mut e,
            r#"SELECT jsonb_set('{"a":1,"b":2}','{a}','9'), jsonb_insert('{"a":1}','{b}','2')"#
        ),
        vec![r#"{"a": 9, "b": 2}|{"a": 1, "b": 2}"#],
        "a mutator's result is canonicalised too — and one grows a one-entry \
         object into a two-entry one"
    );
    assert_eq!(
        vals(
            &mut e,
            r#"SELECT '{"a":1}'::JSONB = '{"a":1.0}'::JSONB, '{"a":1}'::JSONB @> '{"a":1}'::JSONB"#
        ),
        vec!["true|true"],
        "equality reads the canonical form, so 1 and 1.0 compare equal"
    );
}

/// At the size where the canonicaliser ran half a million times.
#[test]
fn round619_scale() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT)").unwrap();
    e.execute("INSERT INTO big SELECT gg FROM generate_series(1, 20000) gg")
        .unwrap();
    assert_eq!(
        vals(
            &mut e,
            r#"SELECT count(DISTINCT (('{"a":'||id||'}')::JSONB)::TEXT) FROM big"#
        ),
        vec!["20000"],
        "every row canonicalises to its own text"
    );
    assert_eq!(
        vals(
            &mut e,
            r#"SELECT count(*) FROM big WHERE (('{"a":'||id||'}')::JSONB ->> 'a')::INT = id"#
        ),
        vec!["20000"]
    );
    assert_eq!(
        vals(
            &mut e,
            r#"SELECT count(*) FROM big WHERE jsonb_build_object('a', id) = ('{"a":'||id||'}')::JSONB"#
        ),
        vec!["20000"],
        "the builder and the cast agree on the canonical form"
    );
}
