//! v7.39 (round 620) — the catalog named several column types wrongly, and a
//! bare `VARCHAR` could not be declared at all.
//!
//! `CREATE TABLE t (x VARCHAR)` failed on `VARCHAR type requires (N)`. The
//! spelled-out `CHARACTER VARYING` — the same type — has always been accepted
//! and read as unbounded. Only the short spelling demanded a length, and the
//! short spelling is the one people write. That is the same asymmetry round
//! 613 closed on the CAST side (`bit varying` folded to `varbit` and
//! `character varying` had no counterpart), here on the DDL side.
//!
//! Then the catalog itself, where a standard introspection query
//! (`pg_attribute` joined to `pg_type`, plus `format_type`) disagreed with
//! `information_schema.columns` about the very same column:
//!
//!   * a REAL column's `atttypid` was 0. A zero type OID does not join to
//!     `pg_type`, so the column DISAPPEARED from the query, and `format_type`
//!     had nothing to name it with and answered `???`. float4's OID has been
//!     in `pg_type` all along; only the column-to-OID direction lacked it;
//!   * a JSONB column's `udt_name` was `text` while its `atttypid` said 3802
//!     one column over — `Jsonb` is its own variant beside `Json` and the
//!     udt table only had the latter;
//!   * a JSON column called itself `jsonb`, in both mappings;
//!   * `BYTEA[]` and `JSON[]` called themselves `text`;
//!   * a declared `varchar(n)` / `char(n)` reported plain text with no
//!     modifier, while `information_schema` (round 248) reported it
//!     correctly; and `numeric(10,2)` reported no modifier either, so
//!     `format_type` spelled both back without their lengths.
//!
//! `atttypmod` was hard-coded to -1 for every column. The packing is PG's,
//! measured against it: a length-carrying character type stores `n + 4`
//! (`varchar(8)` -> 12, `char(4)` -> 8, `char(1)` -> 5), and numeric packs
//! both halves as `((precision << 16) | scale) + 4` (`numeric(10,2)` ->
//! 655366).
//!
//! A 20-column table went from 27 differing lines against live PG18 to 6, and
//! a 15-column one to zero. The 6 that remain are one thing: an UNBOUNDED
//! varchar maps to text because SPG has no unbounded-varchar type, which is
//! the divergence already on the accounted list (r613) and closes only with
//! the varchar/bpchar type epic.

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

/// The DDL that could not be written.
#[test]
fn round620_a_bare_varchar_is_a_type() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE v1 (x VARCHAR)").unwrap();
    e.execute("CREATE TABLE v2 (x VARCHAR, y INT)")
        .expect("and beside another column, where the parser met a comma");
    e.execute("CREATE TABLE v3 (x CHARACTER VARYING)")
        .expect("the long spelling, which always worked");
    e.execute("CREATE TABLE v4 (x VARCHAR(5))")
        .expect("and the bounded one");
    e.execute("INSERT INTO v1 VALUES ('a very long string indeed')")
        .unwrap();
    assert_eq!(
        vals(&mut e, "SELECT x, length(x) FROM v1"),
        vec!["a very long string indeed|25"],
        "unbounded means unbounded"
    );
    assert!(
        e.execute("INSERT INTO v4 VALUES ('123456')").is_err(),
        "and the bounded one still bounds"
    );
}

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE ct (a CHAR(4), b VARCHAR(8), c INT[], d TEXT, e NUMERIC(10,2), \
         f TIMESTAMPTZ, g BOOLEAN, h BYTEA, i UUID, j JSONB, k SMALLINT, l BIGINT, \
         m REAL, n DOUBLE PRECISION, o DATE, p TIME, q INTERVAL, r CHAR, \
         s JSON, t TEXT[], u BYTEA[], v JSON[])",
    )
    .unwrap();
    e
}

/// Every column's type OID, as the standard introspection query reads it.
#[test]
fn round620_atttypid_names_the_declared_type() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.attname, a.atttypid FROM pg_attribute a JOIN pg_class c ON a.attrelid=c.oid \
             WHERE c.relname='ct' AND a.attnum>0 ORDER BY a.attnum"
        ),
        vec![
            "a|1042", // bpchar — was 25
            "b|1043", // varchar — was 25
            "c|1007", "d|25", "e|1700", "f|1184", "g|16", "h|17", "i|2950", "j|3802", "k|21",
            "l|20", "m|700", // float4 — was 0, which joins to nothing
            "n|701", "o|1082", "p|1083", "q|1186", "r|1042", "s|114", "t|1009", "u|1001", "v|199",
        ]
    );
}

/// The modifier, which `format_type` needs to spell the length back out.
#[test]
fn round620_atttypmod_carries_the_declared_length() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT a.attname, a.atttypmod FROM pg_attribute a JOIN pg_class c ON a.attrelid=c.oid \
             WHERE c.relname='ct' AND a.attname IN ('a','b','d','e','m','r') ORDER BY a.attnum"
        ),
        vec![
            "a|8",      // char(4)  -> n + 4
            "b|12",     // varchar(8)
            "d|-1",     // text has no modifier
            "e|655366", // numeric(10,2) -> ((10 << 16) | 2) + 4
            "m|-1", "r|5", // bare CHAR is char(1)
        ]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT format_type(a.atttypid, a.atttypmod) FROM pg_attribute a \
             JOIN pg_class c ON a.attrelid=c.oid WHERE c.relname='ct' \
             AND a.attname IN ('a','b','e','m','r') ORDER BY a.attnum"
        ),
        vec![
            "character(4)",
            "character varying(8)",
            "numeric(10,2)",
            "real",
            "character(1)",
        ],
        "all five used to come back as `text`, bare `numeric`, or `???`"
    );
}

/// `information_schema.columns`, which has to agree with the above.
#[test]
fn round620_udt_name_agrees_with_atttypid() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT column_name, data_type, udt_name FROM information_schema.columns \
             WHERE table_name='ct' AND column_name IN ('a','b','j','m','s','u','v') \
             ORDER BY ordinal_position"
        ),
        vec![
            "a|character|bpchar",
            "b|character varying|varchar",
            "j|jsonb|jsonb", // was text
            "m|real|float4",
            "s|json|json",    // was jsonb, in both columns
            "u|ARRAY|_bytea", // was text
            "v|ARRAY|_json",  // was text
        ]
    );
}
