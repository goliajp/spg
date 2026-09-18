//! 9.0.0 — PostgreSQL's internal single-byte type, `"char"`.
//!
//! Reported by sentori (§5.1): the catalog columns PG declares as
//! `"char"` read `text` here, so a client that types its result columns
//! saw the wrong OID for `relkind`, `contype`, `typtype` and 25 more.
//!
//! Measuring it found the type itself barely reachable:
//!
//! ```text
//!                                     PG 18.6     SPG 8.0.4
//!   pg_typeof('r'::"char")            "char"      unknown
//!   CREATE TABLE t(k "char")          CREATE      syntax error at or near ""char""
//!   (0::int::"char")::text            ''          a NUL byte
//!   ((-128)::int::"char")::int        -128        128
//!   ((-128)::int::"char")::text       \200        È
//!   128::int::"char"                  ERROR       accepted
//! ```
//!
//! Every expectation below is PostgreSQL 18.6's.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .and_then(|r| r.values.first())
            .map(spg_engine::eval::value_to_text)
            .unwrap_or_default(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn run(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
}

/// Every catalog column PostgreSQL declares as `"char"` and SPG has.
const CHAR_COLUMNS: &[(&str, &str)] = &[
    ("pg_class", "relkind"),
    ("pg_class", "relpersistence"),
    ("pg_class", "relreplident"),
    ("pg_attribute", "attalign"),
    ("pg_attribute", "attstorage"),
    ("pg_attribute", "attidentity"),
    ("pg_attribute", "attgenerated"),
    ("pg_attribute", "attcompression"),
    ("pg_constraint", "contype"),
    ("pg_constraint", "confupdtype"),
    ("pg_constraint", "confdeltype"),
    ("pg_constraint", "confmatchtype"),
    ("pg_type", "typtype"),
    ("pg_type", "typcategory"),
    ("pg_type", "typdelim"),
    ("pg_type", "typalign"),
    ("pg_type", "typstorage"),
    ("pg_proc", "prokind"),
    ("pg_proc", "provolatile"),
    ("pg_proc", "proparallel"),
    ("pg_am", "amtype"),
    ("pg_operator", "oprkind"),
    ("pg_collation", "collprovider"),
    ("pg_cast", "castcontext"),
    ("pg_cast", "castmethod"),
    ("pg_database", "datlocprovider"),
];

#[test]
fn every_catalog_column_pg_declares_as_char_is_char_here() {
    let mut e = Engine::new();
    run(
        &mut e,
        "CREATE TABLE cht(id int PRIMARY KEY, s text NOT NULL)",
    );
    for (rel, col) in CHAR_COLUMNS {
        let sql = format!("SELECT pg_typeof({col}) FROM {rel} LIMIT 1");
        assert_eq!(one(&mut e, &sql), "\"char\"", "{rel}.{col}");
    }
}

/// The letters themselves did not move.
#[test]
fn the_catalog_still_reads_the_letters_it_did() {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE cht(id int PRIMARY KEY)");
    assert_eq!(
        one(&mut e, "SELECT relkind FROM pg_class WHERE relname = 'cht'"),
        "r"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT relpersistence FROM pg_class WHERE relname = 'cht'"
        ),
        "p"
    );
    assert_eq!(
        one(&mut e, "SELECT typtype FROM pg_type WHERE typname = 'int4'"),
        "b"
    );
    // The comparison a catalog query is made of still matches.
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM pg_class WHERE relname = 'cht' AND relkind = 'r'"
        ),
        "1"
    );
}

#[test]
fn the_char_type_is_reachable_by_its_written_name() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT pg_typeof('r'::\"char\")"), "\"char\"");
    run(&mut e, "CREATE TABLE chk(k \"char\")");
    run(&mut e, "INSERT INTO chk VALUES ('r')");
    assert_eq!(one(&mut e, "SELECT pg_typeof(k) FROM chk"), "\"char\"");
    assert_eq!(one(&mut e, "SELECT k = 'r' FROM chk"), "true");
    assert_eq!(one(&mut e, "SELECT k::text FROM chk"), "r");
}

#[test]
fn char_is_a_signed_byte_and_prints_the_way_pg_prints_it() {
    let mut e = Engine::new();
    // charout: 0 prints as nothing, ASCII prints itself, a high byte
    // prints as a backslash and three octal digits.
    assert_eq!(one(&mut e, "SELECT (0::int::\"char\")::text"), "");
    assert_eq!(one(&mut e, "SELECT (126::int::\"char\")::text"), "~");
    assert_eq!(one(&mut e, "SELECT ((-128)::int::\"char\")::text"), "\\200");
    assert_eq!(one(&mut e, "SELECT ((-1)::int::\"char\")::text"), "\\377");
    // …and charin reads that form back.
    assert_eq!(one(&mut e, "SELECT ('\\200'::\"char\")::int"), "-128");
    // The integer conversions are signed, both ways.
    assert_eq!(one(&mut e, "SELECT ('a'::\"char\")::int"), "97");
    assert_eq!(one(&mut e, "SELECT ((-128)::int::\"char\")::int"), "-128");
    // Out of range is an error, not a low byte.
    let err = e
        .execute("SELECT 128::int::\"char\"")
        .expect_err("128 is out of range for \"char\"");
    assert!(format!("{err}").contains("\"char\" out of range"), "{err}");
}

/// 9.0.0 — `regclassin` reads an all-digit string as the OID, which is
/// the query `psql \d <table>` ends with.
#[test]
fn a_digit_string_cast_to_regclass_is_that_oid() {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE rgt(id int)");
    // The relation need not exist, exactly as on PG 18.6.
    assert_eq!(one(&mut e, "SELECT '999999'::regclass::text"), "999999");
    assert_eq!(one(&mut e, "SELECT '23'::regtype::text"), "integer");
    // And an oid that IS a relation renders as its name.
    let oid = one(&mut e, "SELECT oid FROM pg_class WHERE relname = 'rgt'");
    assert_eq!(
        one(&mut e, &format!("SELECT '{oid}'::regclass::text")),
        "rgt"
    );
    // The shape psql sends, which used to fail the whole `\d`.
    assert_eq!(
        one(
            &mut e,
            &format!(
                "SELECT count(*) FROM pg_constraint \
                 WHERE confrelid IN (VALUES ('{oid}'::pg_catalog.regclass))"
            )
        ),
        "0"
    );
}

/// Every catalog relation answers `SELECT *` — the row a builder writes
/// has to carry the type its schema declares, and three builders did
/// not. The engine checks this on read, so the query is the check.
#[test]
fn every_catalog_relation_answers_a_select_star() {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE pt(id int PRIMARY KEY, s text)");
    run(&mut e, "CREATE INDEX pti ON pt(s)");
    run(&mut e, "CREATE VIEW pv AS SELECT id FROM pt");
    run(
        &mut e,
        "CREATE FUNCTION pf(a int) RETURNS int AS $$ SELECT a $$ LANGUAGE sql IMMUTABLE",
    );
    run(&mut e, "CREATE TABLE pfk(id int REFERENCES pt(id))");
    run(&mut e, "CREATE TYPE pc AS (a int, b text)");
    run(&mut e, "CREATE DOMAIN pd AS int CHECK (VALUE > 0)");
    run(&mut e, "CREATE POLICY pp ON pt FOR SELECT USING (true)");
    let mut bad = Vec::new();
    for rel in [
        "pg_class",
        "pg_attribute",
        "pg_constraint",
        "pg_type",
        "pg_proc",
        "pg_am",
        "pg_operator",
        "pg_collation",
        "pg_cast",
        "pg_database",
        "pg_depend",
        "pg_policy",
        "pg_trigger",
        "pg_index",
        "pg_namespace",
        "pg_extension",
        "pg_indexes",
        "pg_shdepend",
        "pg_subscription",
        "pg_publication",
        "pg_attrdef",
        "pg_enum",
        "pg_range",
        "pg_rewrite",
    ] {
        if let Err(err) = e.execute(&format!("SELECT * FROM {rel}")) {
            bad.push(format!("{rel}: {err}"));
        }
    }
    assert!(bad.is_empty(), "{bad:#?}");
}

/// A `"char"` is an ordinary value: it sorts, it materialises out of a
/// scalar subquery, and it concatenates with everything PostgreSQL
/// concatenates it with.
#[test]
fn a_char_value_sorts_materialises_and_concatenates() {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE cs(id int PRIMARY KEY)");
    // ORDER BY on a catalog "char" column.
    assert_eq!(
        one(
            &mut e,
            "SELECT contype FROM pg_constraint WHERE conrelid = 'cs'::regclass ORDER BY 1"
        ),
        // 'n' (the NOT NULL) sorts before 'p' (the primary key).
        "n"
    );
    // A scalar subquery that answers one.
    assert_eq!(
        one(
            &mut e,
            "SELECT (SELECT relkind FROM pg_class WHERE relname = 'cs')"
        ),
        "r"
    );
    // PG 18.6 resolves these…
    for (sql, want) in [
        ("SELECT 'r'::\"char\" || 1", "r1"),
        ("SELECT 'r'::\"char\" || 1.5", "r1.5"),
        ("SELECT 'r'::\"char\" || true", "rtrue"),
        ("SELECT 1 || 'r'::\"char\"", "1r"),
        ("SELECT ('r'::\"char\")::text || 'x'", "rx"),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
    // …and refuses these, because both `anynonarray || text` and
    // `text || anynonarray` apply.
    for sql in [
        "SELECT 'r'::\"char\" || 'x'",
        "SELECT 'r'::\"char\" || 'x'::text",
        "SELECT 'r'::\"char\" || 'r'::\"char\"",
        "SELECT 'r'::\"char\" || 'x'::char(2)",
    ] {
        let err = e.execute(sql).expect_err(sql);
        assert!(
            format!("{err}").contains("operator is not unique"),
            "{sql}: {err}"
        );
    }
}
