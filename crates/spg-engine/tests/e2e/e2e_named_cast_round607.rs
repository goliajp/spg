//! v7.39 (round 607) — a cast target spelled by name was re-resolved for
//! every row.
//!
//! `CastTarget` has a settled variant for some spellings and `Named(String)`
//! for the rest, and which one a type gets is an accident of the parser
//! rather than anything about the conversion. Counted with the allocating
//! probe over 200k rows, the same INT source:
//!
//!     id::FLOAT     (settled)   0 allocations a row    7.5 ms
//!     id::FLOAT8    (Named)     8                     40.2
//!     id::REAL      (Named)     8                     44.6
//!     id::NUMERIC   (Named)     8                     45.1
//!     id::NUMERIC(20,6)        10                     55.1
//!
//! `::REAL` produces a `Value::Real(f32)`, which owns nothing on the heap, so
//! those eight allocations had nothing to do with the value. They were the
//! machinery: `eval_cast_arm` CLONED the target for every row (free for a
//! unit variant, a String copy for `Named`), and then five helpers each built
//! their own owned lowercase copy of the name to compare it — `bit_cast_width`,
//! `temporal_typmod`, `cast_catalog_scalar`, the two membership tests in the
//! arm, and `type_name_to_data_type` — plus two Vecs to hold the two numbers
//! in a typmod. All of it re-derived, per row, from a name that is fixed for
//! the whole statement.
//!
//! Now the name is lowercased into a stack buffer where a lowercase form is
//! needed, matched case-insensitively against the static lists where it is
//! not, and the target is passed by reference:
//!
//!     id::NUMERIC          8 -> 1 allocations a row   45.1 -> 26.2 ms
//!     id::REAL             8 -> 1                     44.6 -> 20.9
//!     id::NUMERIC(20,6)   10 -> 1                     55.1 -> 31.8
//!
//! and over pgwire on 500k rows against PG18:
//!
//!     count(id::NUMERIC)                    133.64 -> 65.54    PG  7.70
//!     count(id::NUMERIC / 7)                171.07 -> 110.62   PG 15.30
//!     count((id::NUMERIC/7)::NUMERIC(20,6)) 280.51 -> 178.03   PG 18.12
//!
//! This is a pure refactor of how a NAME is compared, so what it must not do
//! is change which name resolves to what. The pins are that: every spelling
//! in upper, lower and mixed case, the aliases that reach the same type by
//! different routes, typmods with and without a space, the array and
//! pseudotype forms, and the errors an unknown name still raises. All 28
//! shapes were run against the previous binary as well as this one and
//! against live PG18; SPG's answers are byte-identical before and after.
//!
//! Six of those 28 differ from PG18 and every one of them predates this round
//! (verified against the previous binary, PRE == POST): `5::VARBIT(4)`
//! answers where PG refuses int→varbit; `::nosuchtype` and `1::anyarray`
//! word their errors differently; PG decorates errors with LINE/caret;
//! `1::numeric(1000,999)` overflows here and answers there. They are in the
//! ledger, not fixed here.
//!
//! Measured and NOT closed: one allocation a row remains on the named path
//! (`id::FLOAT`, which never enters it, still costs none), and it was not
//! located this round.

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

/// The same type under every case, and through every alias.
#[test]
fn round607_case_and_alias_resolve_the_same() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT 1::NUMERIC, 1::numeric, 1::NuMeRiC, 1::DECIMAL, 1::decimal"
        ),
        vec!["1|1|1|1|1"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT 1.5::REAL, 1.5::real, 1.5::FLOAT4, 1.5::float8, 1.5::FLOAT8"
        ),
        vec!["1.5|1.5|1.5|1.5|1.5"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT 42::TEXT, 42::text, 42::INT4, 42::int8, 42::BIGINT"
        ),
        vec!["42|42|42|42|42"]
    );
    assert_eq!(
        vals(&mut e, "SELECT 1::SMALLINT, 1::smallint, 1::int2"),
        vec!["1|1|1"]
    );
    assert_eq!(
        vals(&mut e, "SELECT 't'::BOOL, 't'::bool, 't'::BOOLEAN"),
        vec!["true|true|true"]
    );
    assert_eq!(
        vals(&mut e, r#"SELECT 'x'::bpchar, 'x'::BPCHAR, 'x'::"char""#),
        vec!["x|x|x"]
    );
    assert_eq!(
        vals(&mut e, "SELECT '2020-01-02'::DATE, '2020-01-02'::date"),
        vec!["2020-01-02|2020-01-02"]
    );
    assert_eq!(
        vals(&mut e, "SELECT '1 day'::INTERVAL, '1 day'::interval"),
        vec!["1 day|1 day"]
    );
    assert_eq!(
        vals(
            &mut e,
            r#"SELECT '{"a":1}'::JSONB, '{"a":1}'::jsonb, '1'::JSON"#
        ),
        vec![r#"{"a": 1}|{"a": 1}|1"#]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT '00000000-0000-0000-0000-000000000001'::UUID, \
             '00000000-0000-0000-0000-000000000001'::uuid"
        ),
        vec!["00000000-0000-0000-0000-000000000001|00000000-0000-0000-0000-000000000001"]
    );
}

/// The typmod forms, which are parsed out of the name itself.
#[test]
fn round607_typmods_still_parse() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT 1.23456::NUMERIC(10,2), 1.23456::numeric(10,2), 1.23456::NUMERIC(10, 2)"
        ),
        vec!["1.23|1.23|1.23"],
        "case, and a space after the comma"
    );
    assert_eq!(
        vals(&mut e, "SELECT 123::NUMERIC(3), 123::NUMERIC(5,0)"),
        vec!["123|123"],
        "a precision with no scale"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT '2020-01-02 03:04:05.678'::TIMESTAMP(0), \
             '2020-01-02 03:04:05.678'::timestamp(2)"
        ),
        vec!["2020-01-02 03:04:06|2020-01-02 03:04:05.68"],
        "the fractional-seconds typmod rounds"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT '03:04:05.678'::TIME(1), '03:04:05.678'::time(0)"
        ),
        vec!["03:04:05.7|03:04:06"]
    );
    assert_eq!(
        vals(&mut e, "SELECT 5::BIT(4), 5::bit(4), 1::BIT"),
        vec!["0101|0101|1"],
        "the bit widths, which are read from the name before anything else"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT 'a'::VARCHAR(3), 'abcdef'::varchar(3), 'ab'::CHAR(4)||'|'"
        ),
        vec!["a|abc|ab|"],
        "a length cap truncates; the engine renders CHAR without its padding"
    );
}

/// Array and pseudotype names, and the errors an unknown one raises.
#[test]
fn round607_arrays_pseudotypes_and_unknown_names() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT '{1,2}'::INT[], '{1,2}'::int4[], '{1.5}'::NUMERIC[]"
        ),
        vec!["{1,2}|{1,2}|{1.5}"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT NULL::NUMERIC IS NULL, NULL::real IS NULL, NULL::anyarray IS NULL"
        ),
        vec!["true|true|true"],
        "a pseudotype IS a type name, so a NULL passes through it"
    );
    let err = e
        .execute("SELECT 1::anyarray")
        .expect_err("a VALUE hits the dummy input function");
    assert!(
        format!("{err}").contains("anyarray"),
        "the message names the type: {err}"
    );
    for spelling in ["SELECT 1::nosuchtype", "SELECT NULL::nosuchtype"] {
        let err = e.execute(spelling).expect_err(spelling);
        assert!(
            format!("{err}").contains("nosuchtype"),
            "an unknown name is refused whatever the operand: {err}"
        );
    }
    assert!(
        e.execute("SELECT 1::NOSUCHTYPE").is_err(),
        "and in upper case too"
    );
    assert!(
        e.execute("SELECT 1::numeric(0,0)").is_err(),
        "a typmod outside PG's range is still refused"
    );
}

/// The NUMERIC edges the resolution feeds: the specials, the widest
/// integers, and the float text round-trip.
#[test]
fn round607_numeric_values_are_unchanged() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT 'Infinity'::NUMERIC, 'NaN'::numeric, 'nan'::NUMERIC"
        ),
        vec!["Infinity|NaN|NaN"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT 1e10::NUMERIC, 0.1::real::numeric, 3.14::NUMERIC"
        ),
        vec!["10000000000|0.1|3.14"],
        "a float keeps its shortest round-trip decimal"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT 9223372036854775807::NUMERIC, (-9223372036854775808)::numeric"
        ),
        vec!["9223372036854775807|-9223372036854775808"]
    );
    assert_eq!(vals(&mut e, "SELECT 1::OID, 1::oid"), vec!["1|1"]);
}

/// At the size where re-resolving the name per row was the cost. Every
/// spelling has to give the column the same values.
#[test]
fn round607_scale() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT)").unwrap();
    e.execute("INSERT INTO big SELECT gg FROM generate_series(1, 20000) gg")
        .unwrap();
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(id::NUMERIC), sum(id::NUMERIC) FROM big"
        ),
        vec!["20000|200010000"]
    );
    assert_eq!(
        vals(&mut e, "SELECT count(id::numeric) FROM big"),
        vals(&mut e, "SELECT count(id::NUMERIC) FROM big"),
        "case does not change the answer"
    );
    assert_eq!(
        vals(&mut e, "SELECT count(id::NUMERIC(20,6)) FROM big"),
        vec!["20000"]
    );
    assert_eq!(
        vals(&mut e, "SELECT min(id::REAL), max(id::FLOAT8) FROM big"),
        vec!["1|20000"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM big WHERE id::NUMERIC / 7 > 100"
        ),
        vec!["19300"]
    );
}
