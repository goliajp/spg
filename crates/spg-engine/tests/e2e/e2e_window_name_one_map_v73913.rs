//! v7.39.13 — the row stream and Describe name a window aggregate the
//! same way.
//!
//! Reported by sentori against 7.39.12. `count(*)` is held as
//! `count_star` so the star arity survives the AST; the projection
//! mapped it back and the extended protocol's Describe did not, so one
//! call had two names:
//!
//! ```text
//!                        row stream   \gdesc     PG 18.6
//!   count(*) OVER ()     count        count_star count
//! ```
//!
//! An ORM reads the Describe name. This is the twin of the type defect
//! 7.39.12 closed in the same synthetic column — the type was fixed
//! there and the name was left, because the two travel by different
//! routes and only one of them was measured.
//!
//! `canonical_function_name` is public now and both routes call it, so
//! there is no second map to fall behind.

use spg_engine::Engine;

/// Through `describe_prepared`, which is the route `\gdesc` and every
/// ORM take — NOT `QueryResult::columns`.
///
/// The first cut of this file asked the row stream, which had the name
/// right all along, so the ablation that removed the fix left it green.
/// A pin has to reach the surface the defect is on.
fn described_name(e: &Engine, sql: &str) -> String {
    let stmt = spg_sql::parser::parse_statement(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let (_, cols) = e.describe_prepared(&stmt);
    cols.first()
        .map_or_else(|| panic!("{sql}: described no columns"), |c| c.name.clone())
}

/// And the row stream's own name, so the two can be compared rather
/// than each asserted against a constant.
fn row_stream_name(e: &mut Engine, sql: &str) -> String {
    let spg_engine::QueryResult::Rows { columns, .. } =
        e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows")
    };
    columns[0].name.clone()
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE w (n int)").unwrap();
    e.execute("INSERT INTO w VALUES (1), (2)").unwrap();
    e
}

#[test]
fn a_window_count_star_is_named_count_like_the_plain_one() {
    let mut e = seeded();
    let plain = described_name(&e, "SELECT count(*) FROM w");
    let win = described_name(&e, "SELECT count(*) OVER () FROM w");
    assert_eq!(plain, "count");
    assert_eq!(
        win, plain,
        "the internal spelling reached the client as a column name"
    );
    // The two routes must agree; disagreeing is the whole defect.
    assert_eq!(
        win,
        row_stream_name(&mut e, "SELECT count(*) OVER () FROM w"),
        "Describe and the row stream named one call differently"
    );
}

/// The neighbours, so a fix that renamed everything to `count` would
/// not pass.
#[test]
fn the_other_window_functions_keep_their_own_names() {
    let e = seeded();
    assert_eq!(described_name(&e, "SELECT sum(n) OVER () FROM w"), "sum");
    assert_eq!(
        described_name(&e, "SELECT row_number() OVER () FROM w"),
        "row_number"
    );
    assert_eq!(
        described_name(&e, "SELECT count(n) OVER () FROM w"),
        "count"
    );
}
