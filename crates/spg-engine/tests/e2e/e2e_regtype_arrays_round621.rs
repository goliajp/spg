//! v7.39 (round 621) — `::regtype` did not know the array types.
//!
//! `1007::regtype` rendered the number `1007` where PG renders `integer[]`,
//! and so did every other array OID. The scalar OIDs were all there. Since the
//! shape this cast exists for is `atttypid::regtype` — asking the catalog what
//! type a column is — an ORM reading it was told the type of every array
//! column was a bare number.
//!
//! A third place already knew: `format_type(1007, -1)` answered `integer[]`
//! correctly the whole time. The array table lived inside `synth_pg_type` as a
//! local, so `pg_type` could be queried for the same fact that `::regtype`
//! could not derive. It is now one crate-level table with three readers, which
//! is the same lesson this round keeps relearning — the sixth time in rounds
//! 620 and 621 that one piece of knowledge had more than one home.
//!
//! Both directions: an array OID reads back as `<element>[]`, and a name
//! ending in `[]` resolves to its array OID.
//!
//! Measured and NOT closed (checklist F22): `'text'::regtype::oid` answers
//! `invalid input syntax for type oid` where PG answers 25 — for SCALARS as
//! much as arrays, so it predates this and is not an array gap. SPG models a
//! regtype as its NAME, so the OID is gone by the time the second cast runs;
//! PG's regtype IS the oid and renders as the name. Closing it means a value
//! variant that carries both, beside the `RegClass` and `RegProc` ones that
//! already do.

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

/// Every array OID `pg_type` carries.
#[test]
fn round621_an_array_oid_reads_back_as_a_type() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT 1007::regtype::text, 1009::regtype::text, 1016::regtype::text, 1005::regtype::text"),
        vec!["integer[]|text[]|bigint[]|smallint[]"]
    );
    assert_eq!(
        vals(&mut e, "SELECT 1000::regtype::text, 1022::regtype::text, 1231::regtype::text, 1001::regtype::text"),
        vec!["boolean[]|double precision[]|numeric[]|bytea[]"]
    );
    assert_eq!(
        vals(&mut e, "SELECT 1182::regtype::text, 1115::regtype::text, 1185::regtype::text, 2951::regtype::text"),
        vec!["date[]|timestamp without time zone[]|timestamp with time zone[]|uuid[]"],
        "the element's canonical spelling carries its spaces into the array's"
    );
    assert_eq!(
        vals(&mut e, "SELECT 199::regtype::text, 3807::regtype::text"),
        vec!["json[]|jsonb[]"]
    );
}

/// The scalars, which were already right and must stay so.
#[test]
fn round621_the_scalars_are_untouched() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT 25::regtype::text, 23::regtype::text, 1042::regtype::text, 1043::regtype::text"),
        vec!["text|integer|character|character varying"]
    );
    assert_eq!(
        vals(&mut e, "SELECT 1114::regtype::text, 1184::regtype::text"),
        vec!["timestamp without time zone|timestamp with time zone"]
    );
    assert_eq!(
        vals(&mut e, "SELECT 999999::regtype::text"),
        vec!["999999"],
        "an OID that names nothing still renders as itself"
    );
}

/// The name direction, and the catalog query this cast exists for.
#[test]
fn round621_a_name_with_brackets_resolves() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT 'integer[]'::regtype::text, 'text[]'::regtype::text"),
        vec!["integer[]|text[]"]
    );
    assert_eq!(
        vals(&mut e, "SELECT 'int4[]'::regtype::text"),
        vec!["integer[]"],
        "an alias canonicalises inside the brackets too"
    );
    let mut e2 = Engine::new();
    e2.execute("CREATE TABLE rt (a INT[], b TEXT[], c INT)").unwrap();
    assert_eq!(
        vals(
            &mut e2,
            "SELECT a.attname, a.atttypid::regtype::text FROM pg_attribute a \
             JOIN pg_class c ON a.attrelid = c.oid WHERE c.relname = 'rt' AND a.attnum > 0 \
             ORDER BY a.attnum"
        ),
        vec!["a|integer[]", "b|text[]", "c|integer"],
        "the shape this cast exists for"
    );
    assert_eq!(
        vals(&mut e2, "SELECT format_type(1007,-1), format_type(1009,-1)"),
        vec!["integer[]|text[]"],
        "and the third reader of the same table still agrees"
    );
}
