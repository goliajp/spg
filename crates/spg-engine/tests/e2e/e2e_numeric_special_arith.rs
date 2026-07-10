//! v7.38 (read01, T6.P3) — NUMERIC special arithmetic + comparison + ordering.
//! PG propagation: NaN wins over everything (incl. div-by-zero, but Inf/0 still
//! errors); Inf-Inf / Inf*0 / Inf/Inf → NaN; finite/Inf → 0; finite%Inf → the
//! dividend; Inf%x → NaN; sign multiplies; total order -Inf < finite < +Inf <
//! NaN; min()/max() follow it. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn t(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            spg_storage::Value::Bool(b) => b.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("rows"),
    }
}

#[test]
fn numeric_special_arithmetic() {
    let mut e = Engine::new();
    assert_eq!(t(&mut e, "SELECT ('NaN'::numeric + 1)::text"), "NaN");
    assert_eq!(
        t(&mut e, "SELECT ('Infinity'::numeric + 1)::text"),
        "Infinity"
    );
    assert_eq!(
        t(
            &mut e,
            "SELECT ('Infinity'::numeric - 'Infinity'::numeric)::text"
        ),
        "NaN"
    );
    assert_eq!(t(&mut e, "SELECT ('Infinity'::numeric * 0)::text"), "NaN");
    assert_eq!(
        t(&mut e, "SELECT ('Infinity'::numeric * (-2))::text"),
        "-Infinity"
    );
    assert_eq!(
        t(
            &mut e,
            "SELECT ('Infinity'::numeric / 'Infinity'::numeric)::text"
        ),
        "NaN"
    );
    assert!(e.execute("SELECT 'Infinity'::numeric / 0").is_err());
    assert_eq!(t(&mut e, "SELECT ('NaN'::numeric / 0)::text"), "NaN");
    assert_eq!(t(&mut e, "SELECT (5 / 'Infinity'::numeric)::text"), "0");
    assert_eq!(t(&mut e, "SELECT (5 % 'Infinity'::numeric)::text"), "5");
    assert_eq!(t(&mut e, "SELECT ('Infinity'::numeric % 5)::text"), "NaN");
    assert_eq!(
        t(&mut e, "SELECT (-('Infinity'::numeric))::text"),
        "-Infinity"
    );
}

#[test]
fn numeric_special_compare_and_order() {
    let mut e = Engine::new();
    assert_eq!(t(&mut e, "SELECT 'NaN'::numeric = 'NaN'::numeric"), "true");
    assert_eq!(
        t(&mut e, "SELECT 'NaN'::numeric > 'Infinity'::numeric"),
        "true"
    );
    assert_eq!(t(&mut e, "SELECT 'NaN'::numeric = 0"), "false");
    assert_eq!(t(&mut e, "SELECT '-Infinity'::numeric < 0"), "true");
    // Total order via ORDER BY, and min()/max().
    assert_eq!(
        t(
            &mut e,
            "SELECT string_agg(x::text, ',' ORDER BY x) FROM (VALUES ('-Infinity'::numeric),('Infinity'::numeric),('NaN'::numeric),(0::numeric),((-5)::numeric)) v(x)"
        ),
        "-Infinity,-5,0,Infinity,NaN"
    );
    assert_eq!(
        t(
            &mut e,
            "SELECT max(x)::text FROM (VALUES ('Infinity'::numeric),(1),('-Infinity'::numeric)) v(x)"
        ),
        "Infinity"
    );
    assert_eq!(
        t(
            &mut e,
            "SELECT min(x)::text FROM (VALUES ('Infinity'::numeric),(1),('-Infinity'::numeric)) v(x)"
        ),
        "-Infinity"
    );
    assert_eq!(
        t(
            &mut e,
            "SELECT max(x)::text FROM (VALUES ('NaN'::numeric),(1),(2)) v(x)"
        ),
        "NaN"
    );
}

#[test]
fn numeric_special_sum_avg() {
    let mut e = Engine::new();
    // sum/avg propagate a special input (NaN wins; ±Inf accumulate; +Inf+-Inf → NaN).
    assert_eq!(
        t(
            &mut e,
            "SELECT sum(x)::text FROM (VALUES (\'NaN\'::numeric),(1),(2)) v(x)"
        ),
        "NaN"
    );
    assert_eq!(
        t(
            &mut e,
            "SELECT avg(x)::text FROM (VALUES (\'NaN\'::numeric),(1),(2)) v(x)"
        ),
        "NaN"
    );
    assert_eq!(
        t(
            &mut e,
            "SELECT sum(x)::text FROM (VALUES (\'Infinity\'::numeric),(1),(2)) v(x)"
        ),
        "Infinity"
    );
    assert_eq!(
        t(
            &mut e,
            "SELECT sum(x)::text FROM (VALUES (\'Infinity\'::numeric),(\'-Infinity\'::numeric)) v(x)"
        ),
        "NaN"
    );
    assert_eq!(
        t(
            &mut e,
            "SELECT avg(x)::text FROM (VALUES (\'Infinity\'::numeric),(1)) v(x)"
        ),
        "Infinity"
    );
    // Finite sums are unaffected.
    assert_eq!(
        t(
            &mut e,
            "SELECT sum(x)::text FROM (VALUES (1.5::numeric),(2.5)) v(x)"
        ),
        "4.0"
    );
    // GROUP BY: only the group with the special is affected.
    assert_eq!(
        t(
            &mut e,
            "SELECT sum(x)::text FROM (VALUES (1,\'NaN\'::numeric),(1,5),(2,3)) v(g,x) WHERE g=2 GROUP BY g"
        ),
        "3"
    );
    assert_eq!(
        t(
            &mut e,
            "SELECT sum(x)::text FROM (VALUES (1,\'NaN\'::numeric),(1,5),(2,3)) v(g,x) WHERE g=1 GROUP BY g"
        ),
        "NaN"
    );
}
