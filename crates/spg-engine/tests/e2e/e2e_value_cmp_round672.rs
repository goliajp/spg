//! Round 672 — two `value_cmp`s, each missing types the other had.
//!
//! `orderby::value_cmp` and `aggregate::value_cmp` are two independently
//! written comparison matrices with the same name. A census of which
//! `Value` variants each one names found them DIVERGED rather than
//! duplicated: five variants only in the first, eight only in the second,
//! seventeen shared.
//!
//! The census predicted the failures and measurement confirmed two of them,
//! both silent:
//!
//!   * `ORDER BY time_col` did not sort. Three rows inserted 09/02/05 came
//!     back 05,09,02; PG gives 02,05,09. `Time` was in the aggregate matrix
//!     and not the orderby one.
//!   * `min`/`max` over a `CHAR(n)` column returned the FIRST row for both.
//!     `BpChar` was in the orderby matrix and not the aggregate one.
//!
//! Round 641 had already written down what this failure looks like —
//! "value_cmp had no arms for these, so they fell to `_ => Equal` and kept
//! the first row" — for a different set of types. The shape recurred
//! because the two matrices were never converged, which
//! `docs/COLLATION_RFC.md` §3 records as the structural fix.
//!
//! The other eleven predicted types measured fine, so the census produces
//! candidates and only measurement produces findings.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
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
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

fn seed(e: &mut Engine) {
    e.execute("CREATE TABLE vc(t TIME, c CHAR(4), i INET, m MONEY, b BYTEA)")
        .unwrap();
    // Inserted deliberately out of order, and NOT in reverse either, so a
    // matrix that returns Equal cannot accidentally look sorted.
    e.execute(
        "INSERT INTO vc VALUES \
         ('09:00:00','dddd','10.0.0.9','9.00','\\x09'), \
         ('02:00:00','aaaa','10.0.0.2','2.00','\\x02'), \
         ('05:00:00','cccc','10.0.0.5','5.00','\\x05')",
    )
    .unwrap();
}

/// PG18-verified. `ORDER BY` has to sort every ordered type, not the ones
/// that happen to be named in one of the two matrices.
#[test]
fn round672_order_by_sorts_time_and_its_neighbours() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(
        one(&mut e, "SELECT t FROM vc ORDER BY t"),
        "02:00:00,05:00:00,09:00:00"
    );
    assert_eq!(one(&mut e, "SELECT c FROM vc ORDER BY c"), "aaaa,cccc,dddd");
    assert_eq!(
        one(&mut e, "SELECT host(i) FROM vc ORDER BY i"),
        "10.0.0.2,10.0.0.5,10.0.0.9"
    );
    assert_eq!(one(&mut e, "SELECT m FROM vc ORDER BY m"), "$2.00,$5.00,$9.00");
    // DESC too: a matrix returning Equal looks identical either way, so the
    // ascending assertion alone would not catch a regression to it.
    assert_eq!(
        one(&mut e, "SELECT t FROM vc ORDER BY t DESC"),
        "09:00:00,05:00:00,02:00:00"
    );
}

/// PG18-verified. min/max go through the OTHER matrix, so they need their
/// own coverage of the same types.
#[test]
fn round672_min_max_cover_the_same_types() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(one(&mut e, "SELECT min(c), max(c) FROM vc"), "aaaa|dddd");
    assert_eq!(one(&mut e, "SELECT min(t), max(t) FROM vc"), "02:00:00|09:00:00");
    assert_eq!(one(&mut e, "SELECT min(m), max(m) FROM vc"), "$2.00|$9.00");
    // A CHAR(n) compares without its padding, as on PG.
    assert_eq!(
        one(&mut e, "SELECT min(c) = 'aaaa', max(c) = 'dddd' FROM vc"),
        "true|true"
    );
}

/// The property that actually failed, stated directly: a comparison must
/// not answer Equal for two values that differ. Both matrices are exercised
/// on the same data, so a type that reaches one and not the other shows up
/// as a disagreement between these two answers.
#[test]
fn round672_the_two_matrices_agree_on_extremes() {
    let mut e = Engine::new();
    seed(&mut e);
    for col in ["t", "c", "i", "m", "b"] {
        let by_sort = one(&mut e, &format!("SELECT {col} FROM vc ORDER BY {col} LIMIT 1"));
        let by_min = one(&mut e, &format!("SELECT min({col}) FROM vc"));
        assert_eq!(
            by_sort, by_min,
            "ORDER BY {col} and min({col}) disagree — one matrix is missing the type"
        );
    }
}
