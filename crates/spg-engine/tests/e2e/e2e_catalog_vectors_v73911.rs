//! v7.39.11 — the catalog's vector columns are `int2vector` /
//! `oidvector`, which are arrays.
//!
//! Reported by sentori against 7.39.10, and long-standing: 7.38.21
//! behaves the same, so this is not something the 7.39 line did.
//! `pg_index.indkey`, `indclass`, `indoption`, `indcollation` and
//! `pg_proc.proargtypes` were `text` holding the vector's printed form.
//! The printing was right and every array operation over them raised:
//!
//! ```text
//!   a.attnum = ANY (i.indkey)      ERROR: ANY/ALL right-hand side must be an array, got text
//!   unnest(indkey)                 ERROR: unnest() expects an array argument, got text
//!   array_position(indkey, …)      ERROR: array_position() first arg must be an array, got text
//!   indkey[0]                      ERROR: subscript target must be an array, got text
//! ```
//!
//! `= ANY (i.indkey)` is not an exotic spelling; it is what Django's
//! introspection, Rails' schema dumper, sqlalchemy and every
//! hand-written schema-diff query use to ask which columns an index
//! covers.
//!
//! Everything here was measured on PostgreSQL 18.6 first, with
//! `a int PRIMARY KEY, b text, c int` and `CREATE INDEX … (b, c)`:
//!
//! ```text
//!   pg_typeof(indkey)                        int2vector
//!   pg_typeof(indclass) / (indcollation)     oidvector
//!   indkey::text            for (b, c)       2 3
//!   array_position(indkey, 2::smallint)      0      <- vectors subscript from 0
//!   indkey[0]                                2
//!   pg_typeof(proargtypes)                   oidvector
//!   pg_attribute.attndims declared type      smallint
//! ```

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows");
    };
    rows.iter()
        .map(|r| {
            r.values
                .iter()
                .map(spg_engine::eval::value_to_text)
                .collect()
        })
        .collect()
}

fn one(e: &mut Engine, sql: &str) -> String {
    let r = rows(e, sql);
    assert_eq!(r.len(), 1, "{sql}: expected one row, got {}", r.len());
    r[0][0].clone()
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE cv (a int PRIMARY KEY, b text, c int)")
        .unwrap();
    e.execute("CREATE INDEX cv_bc ON cv (b, c)").unwrap();
    e
}

const OF_CV_BC: &str =
    "FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = 'cv_bc'";

#[test]
fn the_four_index_vectors_carry_pgs_own_type_names() {
    let mut e = seeded();
    let r = rows(
        &mut e,
        "SELECT pg_typeof(indkey)::text, pg_typeof(indclass)::text, \
         pg_typeof(indoption)::text, pg_typeof(indcollation)::text FROM pg_index LIMIT 1",
    );
    assert_eq!(
        r[0],
        vec!["int2vector", "oidvector", "int2vector", "oidvector"]
    );
}

#[test]
fn a_vector_still_prints_the_way_pg_prints_it() {
    // Space-separated, no braces — the array types print `{2,3}`, and a
    // tool that reads this column as text has been reading `2 3` since
    // before it was typed.
    let mut e = seeded();
    assert_eq!(
        one(&mut e, &format!("SELECT indkey::text {OF_CV_BC}")),
        "2 3"
    );
}

#[test]
fn any_over_indkey_is_the_query_every_schema_tool_writes() {
    let mut e = seeded();
    let n = one(
        &mut e,
        "SELECT count(*) FROM pg_attribute a JOIN pg_index i ON i.indrelid = a.attrelid \
         WHERE a.attnum = ANY (i.indkey)",
    );
    // Three: `a` under the primary key's index, and `b` and `c` under
    // cv_bc. What matters is that it answers at all.
    assert_eq!(n, "3");
}

#[test]
fn unnest_walks_a_vector() {
    let mut e = seeded();
    let r = rows(&mut e, &format!("SELECT unnest(indkey)::text {OF_CV_BC}"));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], "2");
    assert_eq!(r[1][0], "3");
}

#[test]
fn array_position_counts_from_the_vectors_own_lower_bound() {
    // PG answers 0 here, not 1: `int2vector` subscripts start at 0.
    let mut e = seeded();
    assert_eq!(
        one(
            &mut e,
            &format!("SELECT array_position(indkey, 2::smallint) {OF_CV_BC}")
        ),
        "0"
    );
}

#[test]
fn subscripting_starts_at_zero() {
    let mut e = seeded();
    assert_eq!(
        one(&mut e, &format!("SELECT indkey[0]::text {OF_CV_BC}")),
        "2"
    );
    assert_eq!(
        one(&mut e, &format!("SELECT indkey[1]::text {OF_CV_BC}")),
        "3"
    );
}

#[test]
fn an_ordinary_array_still_subscripts_from_one() {
    // The negative control: the lower bound moved for two types, not
    // for every type.
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT (ARRAY[7,8])[1]::text"), "7");
    assert_eq!(one(&mut e, "SELECT array_position(ARRAY[7,8], 8)"), "2");
}

#[test]
fn proargtypes_is_an_oidvector() {
    let mut e = Engine::new();
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_typeof(proargtypes)::text FROM pg_proc LIMIT 1"
        ),
        "oidvector"
    );
}

#[test]
fn unnest_reaches_every_array_family_type_now() {
    // The arms named int / bigint / text / json and stopped, so this
    // raised "expects an array argument, got smallint[]" — naming the
    // type it had just been handed. Found while closing the vectors.
    let mut e = Engine::new();
    let r = rows(&mut e, "SELECT unnest(ARRAY[1,2]::smallint[])::text");
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], "1");
}

#[test]
fn attndims_is_declared_smallint() {
    let mut e = Engine::new();
    assert_eq!(
        one(
            &mut e,
            "SELECT format_type(atttypid, NULL) FROM pg_attribute \
             WHERE attrelid = 'pg_attribute'::regclass AND attname = 'attndims'"
        ),
        "smallint"
    );
}
