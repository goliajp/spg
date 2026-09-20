//! 9.0.0 — an equi-join whose two sides are different NUMERIC types
//! finds its rows.
//!
//! The hash join's canonical-string key tagged the width: `1::int` was
//! `I1|` and `1::bigint` was `B1|`, so the two never met in the table.
//! A single integer key took a typed i64 lane and was unaffected, which
//! is why this survived: it needed a key with two columns, or a numeric
//! or float side. Measured against PostgreSQL 18.6:
//!
//! ```text
//!   ja(x int, y int) JOIN jb(x bigint, y bigint)
//!     ON ja.x = jb.x                      PG 2   SPG 2   (i64 lane)
//!     ON ja.x = jb.x AND ja.y = jb.y      PG 2   SPG 0
//!     USING (x, y)                        PG 2   SPG 0
//!   int ⋈ numeric, int ⋈ float8, int ⋈ smallint, two columns
//!                                         PG 2   SPG 0
//! ```
//!
//! The float rules are PostgreSQL's own and are pinned here too: a
//! number compared with a float is compared IN float8, so `0.1::real`
//! does NOT equal `0.1::float8` (the real widens to 0.10000000149…)
//! while `0.1::numeric` does, `NaN` equals `NaN`, and `-0` equals `0`.

use spg_engine::{Engine, QueryResult};

fn run(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn one(e: &mut Engine, sql: &str) -> String {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    let r = rows.into_iter().next().expect("one row");
    spg_engine::eval::value_to_text(r.values.first().expect("one column"))
}

#[test]
fn a_two_column_key_joins_across_integer_widths_and_numeric_kinds() {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE ja(x int, y int)");
    run(&mut e, "CREATE TABLE jb(x bigint, y bigint)");
    run(&mut e, "CREATE TABLE jn(x numeric, y numeric)");
    run(&mut e, "CREATE TABLE jf(x float8, y float8)");
    run(&mut e, "CREATE TABLE js(x smallint, y smallint)");
    for t in ["ja", "jb", "jn", "jf", "js"] {
        run(&mut e, &format!("INSERT INTO {t} VALUES (1,2),(3,4)"));
    }

    for peer in ["jb", "jn", "jf", "js"] {
        // One column: the typed integer lane, which always worked.
        assert_eq!(
            one(
                &mut e,
                &format!("SELECT count(*) FROM ja JOIN {peer} p ON ja.x = p.x")
            ),
            "2",
            "one-column key against {peer}"
        );
        // Two columns: the canonical-string lane, which did not.
        assert_eq!(
            one(
                &mut e,
                &format!("SELECT count(*) FROM ja JOIN {peer} p ON ja.x = p.x AND ja.y = p.y")
            ),
            "2",
            "two-column key against {peer}"
        );
        // The same key written as a WHERE equality, which takes the
        // same lane through a different door.
        assert_eq!(
            one(
                &mut e,
                &format!("SELECT count(*) FROM ja, {peer} p WHERE ja.x = p.x AND ja.y = p.y")
            ),
            "2",
            "WHERE-equality key against {peer}"
        );
    }

    // `USING` names the columns instead of writing the equalities.
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM ja JOIN jb USING (x, y)"),
        "2"
    );

    // And a key that should NOT match still does not: the join is not
    // simply matching everything now.
    run(&mut e, "INSERT INTO jb VALUES (5,6)");
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM ja JOIN jb ON ja.x = jb.x AND ja.y = jb.y"
        ),
        "2"
    );
}

#[test]
fn a_float_side_compares_in_float8_as_pg_does() {
    let mut e = Engine::new();
    run(
        &mut e,
        "CREATE TABLE fa(k int, x real, y numeric, z float8)",
    );
    run(
        &mut e,
        "CREATE TABLE fb(k int, x float8, y float8, z float8)",
    );
    run(
        &mut e,
        "INSERT INTO fa VALUES (1, 0.1, 0.1, 'NaN'), (2, 0.5, 0.5, -0.0), (3, 1, 1, 1)",
    );
    run(
        &mut e,
        "INSERT INTO fb VALUES (1, 0.1, 0.1, 'NaN'), (2, 0.5, 0.5, 0.0), (3, 1, 1, 1)",
    );
    // real 0.1 widens to 0.10000000149…, which float8 0.1 is not — so
    // row 1 drops out. Rows 2 and 3 are exact in binary.
    assert_eq!(
        one(
            &mut e,
            "SELECT string_agg(fa.k::text, ',' ORDER BY fa.k) \
             FROM fa JOIN fb ON fa.k = fb.k AND fa.x = fb.x"
        ),
        "2,3"
    );
    // numeric against float8 compares in float8, so 0.1 meets 0.1.
    assert_eq!(
        one(
            &mut e,
            "SELECT string_agg(fa.k::text, ',' ORDER BY fa.k) \
             FROM fa JOIN fb ON fa.k = fb.k AND fa.y = fb.y"
        ),
        "1,2,3"
    );
    // NaN equals NaN and -0 equals 0, both PostgreSQL's float8 rules.
    assert_eq!(
        one(
            &mut e,
            "SELECT string_agg(fa.k::text, ',' ORDER BY fa.k) \
             FROM fa JOIN fb ON fa.k = fb.k AND fa.z = fb.z"
        ),
        "1,2,3"
    );
}
