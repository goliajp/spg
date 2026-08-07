//! read01 round 336 (V58) — the view definition PG prints.
//!
//! `pg_get_viewdef` already produced PG's layout for a plain single-table
//! SELECT; anything carrying WHERE + ORDER BY, GROUP BY or HAVING fell back
//! to the stored one-line body. Two further things showed up on the way:
//!
//!   * SPG's internal `count_star()` spelling reached the reflected
//!     definition, where PG says `count(*)` — the same class of leak round
//!     323 removed from error messages;
//!   * `information_schema.views.view_definition` returned the stored body
//!     rather than the deparsed text `pg_get_viewdef` gives, so an ORM
//!     reading views through the standard surface saw both problems.
//!
//! PG 18.4 measured, verbatim:
//!
//! ```text
//!  SELECT id,
//!     v
//!    FROM v58
//!   WHERE (v > 1)
//!   ORDER BY id;
//!
//!  SELECT count(*) AS n,
//!     v
//!    FROM v58
//!   GROUP BY v
//!  HAVING (count(*) > 1);
//! ```
//!
//! Note the indents: two spaces for WHERE / GROUP BY / ORDER BY, ONE for
//! HAVING, three for FROM, four for a continued projection column.

use spg_engine::Engine;
use spg_storage::Value;

fn text_of(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        spg_engine::QueryResult::Rows { rows, .. } => {
            match rows.first().and_then(|r| r.values.first()) {
                Some(Value::Text(t)) => t.to_string(),
                other => panic!("`{sql}` did not return text: {other:?}"),
            }
        }
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE v58 (id INT, v INT, nm TEXT)")
        .unwrap();
    e.execute("CREATE VIEW v1 AS SELECT id, v FROM v58")
        .unwrap();
    e.execute("CREATE VIEW v2 AS SELECT id, v FROM v58 WHERE v > 1 ORDER BY id")
        .unwrap();
    e.execute("CREATE VIEW v4 AS SELECT count(*) AS n, v FROM v58 GROUP BY v HAVING count(*) > 1")
        .unwrap();
    e
}

#[test]
fn a_plain_view_keeps_pgs_layout() {
    let mut e = fixture();
    assert_eq!(
        text_of(&mut e, "SELECT pg_get_viewdef('v1')"),
        " SELECT id,\n    v\n   FROM v58;",
    );
}

#[test]
fn where_and_order_by_get_their_own_lines() {
    let mut e = fixture();
    assert_eq!(
        text_of(&mut e, "SELECT pg_get_viewdef('v2')"),
        " SELECT id,\n    v\n   FROM v58\n  WHERE (v > 1)\n  ORDER BY id;",
    );
}

/// GROUP BY sits two spaces in, HAVING one — and `count(*)` is spelled
/// PG's way, not SPG's internal `count_star()`.
#[test]
fn group_by_and_having_match_pgs_indents_and_spelling() {
    let mut e = fixture();
    let def = text_of(&mut e, "SELECT pg_get_viewdef('v4')");
    assert_eq!(
        def,
        " SELECT count(*) AS n,\n    v\n   FROM v58\n  GROUP BY v\n HAVING (count(*) > 1);",
    );
    assert!(
        !def.contains("count_star"),
        "the engine's internal spelling must not reach a client: {def}"
    );
}

/// The standard introspection surface gives the same text.
#[test]
fn information_schema_views_agrees_with_pg_get_viewdef() {
    let mut e = fixture();
    for v in ["v1", "v2", "v4"] {
        let direct = text_of(&mut e, &format!("SELECT pg_get_viewdef('{v}')"));
        let via_info = text_of(
            &mut e,
            &format!(
                "SELECT view_definition FROM information_schema.views WHERE table_name = '{v}'"
            ),
        );
        assert_eq!(via_info, direct, "for view {v}");
        assert!(!via_info.contains("count_star"), "for view {v}");
    }
}
