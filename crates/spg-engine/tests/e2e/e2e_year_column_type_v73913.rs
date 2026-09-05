//! v7.39.13 — a `YEAR` column has a type, and `timetz` reports the same
//! type on all three surfaces that name one.
//!
//! Found while asking which column types may lead a composite index.
//! `year` is MySQL's, and on MySQL 9.7.2 it is an integer type in every
//! way that shows:
//!
//! ```text
//!                                   MySQL 9.7.2   SPG 7.39.12
//!   WHERE k = 2007                  1,3           ERROR: operator does
//!                                                 not exist: unknown =
//!                                                 integer
//!   WHERE k > 2007                  2             the same error
//!   pg_typeof(k)                    —             unknown
//!   information_schema … data_type  year          user-defined
//! ```
//!
//! The column's type had landed — the parser maps `year` and the values
//! store as years — and every surface that NAMES a type was missing it,
//! so the comparability gate saw `unknown` on the left and refused.
//! `types_unify` put it in no family at all; it is in the numeric one
//! now, which is where `=`, `>`, `ORDER BY` and `MAX` put it on 9.7.2.
//!
//! `timetz` is the same shape one step along: `pg_typeof` and
//! `format_type` both answered `time with time zone` for a column whose
//! `information_schema.columns.data_type` said `USER-DEFINED`. Three
//! surfaces, one column, two answers. PostgreSQL 18.6 says `time with
//! time zone` on all three.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows")
    };
    rows.iter()
        .map(|r| {
            r.values
                .iter()
                .map(spg_engine::eval::value_to_text)
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.set_mysql_wire_session();
    e.execute("CREATE TABLE t (id int, k year)").unwrap();
    e.execute("INSERT INTO t VALUES (1,2007),(2,2008),(3,2007)")
        .unwrap();
    e
}

/// Every row measured on MySQL 9.7.2 first.
#[test]
fn a_year_column_compares_against_an_integer() {
    let mut e = seeded();
    assert_eq!(
        rows(&mut e, "SELECT id FROM t WHERE k = 2007 ORDER BY id"),
        ["1", "3"]
    );
    assert_eq!(
        rows(&mut e, "SELECT id FROM t WHERE k > 2007 ORDER BY id"),
        ["2"]
    );
    assert_eq!(
        rows(&mut e, "SELECT k FROM t ORDER BY k"),
        ["2007", "2007", "2008"]
    );
    assert_eq!(rows(&mut e, "SELECT max(k), min(k) FROM t"), ["2008|2007"]);
}

/// The other half of "it is a number": the constructs that have to
/// resolve two types to one. Measured on MySQL 9.7.2, which unifies a
/// year with an integer in every one of them.
#[test]
fn a_year_unifies_with_an_integer() {
    let mut e = seeded();
    assert_eq!(
        rows(
            &mut e,
            "SELECT x FROM (SELECT k AS x FROM t UNION SELECT 5) u ORDER BY x"
        ),
        ["5", "2007", "2008"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT c FROM (SELECT CASE WHEN id = 1 THEN k ELSE 5 END AS c FROM t) v \
             GROUP BY c ORDER BY c"
        ),
        ["5", "2007"]
    );
    assert_eq!(
        rows(&mut e, "SELECT coalesce(k, 5) FROM t LIMIT 1"),
        ["2007"]
    );
    assert_eq!(
        rows(&mut e, "SELECT greatest(k, 5) FROM t LIMIT 1"),
        ["2007"]
    );
}

#[test]
fn a_year_column_names_its_type() {
    let mut e = seeded();
    assert_eq!(rows(&mut e, "SELECT pg_typeof(k) FROM t LIMIT 1"), ["year"]);
    assert_eq!(
        rows(
            &mut e,
            "SELECT data_type FROM information_schema.columns \
             WHERE table_name = 't' AND column_name = 'k'"
        ),
        ["year"]
    );
}

/// PostgreSQL 18.6 answers `time with time zone` on all three.
#[test]
fn the_three_surfaces_agree_about_a_timetz_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id int, k timetz)").unwrap();
    e.execute("INSERT INTO u VALUES (1,'07:00:00+00')").unwrap();
    assert_eq!(
        rows(&mut e, "SELECT pg_typeof(k) FROM u"),
        ["time with time zone"]
    );
    assert_eq!(
        rows(&mut e, "SELECT pg_typeof(NULL::timetz)"),
        ["time with time zone"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT format_type(atttypid, atttypmod) FROM pg_attribute \
             WHERE attrelid = 'u'::regclass AND attname = 'k'"
        ),
        ["time with time zone"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT data_type FROM information_schema.columns \
             WHERE table_name = 'u' AND column_name = 'k'"
        ),
        ["time with time zone"]
    );
}
