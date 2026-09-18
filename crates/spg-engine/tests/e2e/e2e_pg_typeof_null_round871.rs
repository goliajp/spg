//! `pg_typeof(NULL::t)` names the type, as PG does.
//!
//! It used to answer `unknown` for most types, which reads as a NULL
//! problem and is not one: `NULL::uuid` was right while `NULL::text`
//! was wrong. The static-type path had been there since round 640 —
//! only the `DataType` → PG-name table it consults was short, twenty
//! entries, and everything else fell through to `unknown`.
//!
//! Round 870 found this while sampling the readiness doc's type
//! section. The first reading — nineteen types answering `unknown` —
//! looks like nineteen missing types; the same casts with real values
//! all matched PG, so the types were never the problem.

use spg_engine::{Engine, QueryResult};

fn typeof_of(e: &mut Engine, cast: &str) -> String {
    let sql = alloc_sql(cast);
    match e.execute(&sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            // 9.0.0 — `pg_typeof` answers a `regtype`, which carries the oid beside the name (PostgreSQL 18.6 describes it as `regtype` and sends the oid in binary). What this pin means is the NAME.
            spg_storage::Value::Text(s) => s.to_string(),
            spg_storage::Value::RegClass(_, s)
            | spg_storage::Value::RegType(_, s)
            | spg_storage::Value::RegProc(_, s) => s.to_string(),
            other => panic!("{cast}: {other:?}"),
        },
        other => panic!("{cast}: {other:?}"),
    }
}

fn alloc_sql(cast: &str) -> String {
    format!("SELECT pg_typeof(NULL::{cast})")
}

#[test]
fn a_null_cast_reports_the_type_it_was_cast_to() {
    let mut e = Engine::new();
    // PG18's own answers, taken by running pg_typeof there.
    for (cast, want) in [
        ("text", "text"),
        ("inet", "inet"),
        ("cidr", "cidr"),
        ("macaddr", "macaddr"),
        ("macaddr8", "macaddr8"),
        ("xml", "xml"),
        ("money", "money"),
        ("point", "point"),
        ("line", "line"),
        ("lseg", "lseg"),
        ("box", "box"),
        ("path", "path"),
        ("polygon", "polygon"),
        ("circle", "circle"),
        ("varbit", "bit varying"),
        ("bit(4)", "bit"),
        ("int4[]", "integer[]"),
        ("text[]", "text[]"),
        ("int4multirange", "int4multirange"),
        ("int8multirange", "int8multirange"),
        // These were already right, and are here so a future change to
        // the table cannot quietly drop them.
        ("uuid", "uuid"),
        ("interval", "interval"),
        ("int4", "integer"),
        // `char(n)` is `character`; SPG maps it and PG's one-byte
        // `"char"` onto one DataType, so this arm answers for the
        // declared type — see the residual noted below.
        ("char(5)", "character"),
        ("varchar(3)", "character varying"),
    ] {
        assert_eq!(typeof_of(&mut e, cast), want, "pg_typeof(NULL::{cast})");
    }
}

/// The one shape that still differs, pinned so it is a known residual
/// rather than a surprise: PG has `"char"` as a distinct one-byte type
/// and SPG folds it into `Char(u32)` alongside `character(n)`. Naming
/// this arm `"char"` would fix the rare spelling and break the common
/// one, so `character` is what it answers.
#[test]
fn the_one_byte_char_type_is_not_distinguished_yet() {
    let mut e = Engine::new();
    // 9.0.0 — and a future fix did come here and say so: `"char"` is
    // its own type now, and `pg_typeof` names it the way PG 18.6 does.
    assert_eq!(
        typeof_of(&mut e, "\"char\""),
        "\"char\"",
        "PG says \"char\" and so does SPG"
    );
}
