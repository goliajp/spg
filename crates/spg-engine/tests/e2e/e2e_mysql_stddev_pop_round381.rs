//! read01 round 381 (MySQL differential) — bare STDDEV / VARIANCE are the
//! POPULATION statistics under the MySQL dialect, not the sample ones.
//!
//! MariaDB 11: `STDDEV(x)` == `STDDEV_POP(x)` and `VARIANCE(x)` ==
//! `VAR_POP(x)`, while PG's bare `STDDEV` / `VARIANCE` are the SAMPLE
//! forms. Over {10,20,30,5,15} the population variance is 74 and the
//! sample variance is 92.5 — SPG returned 92.5 for a MySQL `VARIANCE(v)`,
//! a silently wrong statistic. The explicit `_pop` / `_samp` spellings
//! are unchanged in both dialects, and a window `STDDEV(x) OVER (...)`
//! follows the same rule. PG keeps the sample default.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn seed(dialect_mysql: bool) -> Engine {
    let mut e = Engine::new();
    if dialect_mysql {
        e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    }
    e.execute("CREATE TABLE w (v INT)").unwrap();
    e.execute("INSERT INTO w VALUES (10),(20),(30),(5),(15)")
        .unwrap();
    e
}

fn num(e: &mut Engine, sql: &str) -> f64 {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::Numeric { scaled, scale, .. } => {
                *scaled as f64 / 10f64.powi(i32::from(*scale))
            }
            Value::Float(x) => *x,
            other => panic!("`{sql}` not numeric: {other:?}"),
        },
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

/// Under the MySQL dialect, bare VARIANCE / STDDEV are population.
#[test]
fn mysql_bare_forms_are_population() {
    let mut e = seed(true);
    assert!((num(&mut e, "SELECT VARIANCE(v) FROM w") - 74.0).abs() < 1e-9);
    assert!((num(&mut e, "SELECT STDDEV(v) FROM w") - 74.0_f64.sqrt()).abs() < 1e-6);
    // The explicit spellings are unaffected.
    assert!((num(&mut e, "SELECT VAR_POP(v) FROM w") - 74.0).abs() < 1e-9);
    assert!((num(&mut e, "SELECT VAR_SAMP(v) FROM w") - 92.5).abs() < 1e-9);
    assert!((num(&mut e, "SELECT STDDEV_SAMP(v) FROM w") - 92.5_f64.sqrt()).abs() < 1e-6);
}

/// A window STDDEV follows the same population rule.
#[test]
fn window_stddev_is_population() {
    let mut e = seed(true);
    assert!(
        (num(&mut e, "SELECT STDDEV(v) OVER () FROM w LIMIT 1") - 74.0_f64.sqrt()).abs() < 1e-6
    );
}

/// A PostgreSQL session keeps the sample default.
#[test]
fn postgres_bare_forms_are_sample() {
    let mut p = seed(false);
    assert!((num(&mut p, "SELECT VARIANCE(v) FROM w") - 92.5).abs() < 1e-9);
    assert!((num(&mut p, "SELECT STDDEV(v) FROM w") - 92.5_f64.sqrt()).abs() < 1e-6);
}
