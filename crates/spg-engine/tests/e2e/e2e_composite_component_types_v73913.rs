//! v7.39.13 — which column types may be a component of a composite
//! B-tree, asked of every one of them rather than of a hand-kept list.
//!
//! `multi_component_type_ok` was a `matches!` over eleven names, so the
//! other sixty-three `DataType`s answered "no" by falling off the end
//! and nothing could say which of them meant it. An index over a type
//! that falls off is still CREATED — it stays a single-column B-tree on
//! the leading column with the extra positions recorded and unused — so
//! the miss is silent: every answer is right, and
//! `WHERE lead = ? ORDER BY next DESC LIMIT n` sorts the whole table.
//!
//! Two of the misses arrived as separate reports and were one hole.
//! `timestamptz` came from sentori's production access path. `numeric`
//! came from this version's own perf gate, hours later, with an index
//! over `(n numeric, id)`:
//!
//! ```text
//!   rows     WHERE n = 1.23 ORDER BY id DESC LIMIT 20
//!   10,000   SPG 0.497-0.520 ms   PG 18.6 0.183-0.234 ms
//!   50,000   SPG 0.975-0.991 ms   PG 18.6 0.195-0.439 ms
//! ```
//!
//! Twenty rows behind a seek do not cost twice as much on five times
//! the table.
//!
//! So this file asks the QUESTION rather than the instance: for each
//! type that can key, put it at the front of a composite and make the
//! walk answer in order. The gate is an exhaustive match now, so a new
//! `DataType` does not compile until someone answers for it; these rows
//! are the other half — that a `true` answer actually walks.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows")
    };
    rows.iter()
        .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
        .collect()
}

/// 8 groups x 40 rows over a leading column of type `ty`, where
/// `lit(g)` spells group `g` as a literal of that type. Interleaved, so
/// a walk that fell back to a scan would still answer and only the
/// order of a LIMIT would tell.
fn seeded(ty: &str, lit: impl Fn(u32) -> String) -> Engine {
    let mut e = Engine::new();
    e.execute(&format!(
        "CREATE TABLE ev (id bigint NOT NULL, k {ty} NOT NULL, seq int NOT NULL)"
    ))
    .unwrap();
    let mut sql = String::from("INSERT INTO ev VALUES ");
    for g in 0..320u32 {
        if g > 0 {
            sql.push(',');
        }
        sql.push_str(&format!("({}, {}, {})", g, lit(g % 8), g));
    }
    e.execute(&sql).unwrap();
    e.execute("CREATE INDEX ev_k_seq ON ev (k, seq)").unwrap();
    e
}

/// The same three questions for every type: the last rows of one group
/// in order, the plan that produced them, and that the group did not
/// leak into its neighbours.
fn walks_in_order(ty: &str, lit: impl Fn(u32) -> String) {
    let probe = lit(7);
    let mut e = seeded(ty, lit);
    // group 7 holds g where g % 8 == 7: … 319, 311, 303, 295, 287
    let got = rows(
        &mut e,
        &format!("SELECT id FROM ev WHERE k = {probe} ORDER BY seq DESC LIMIT 5"),
    );
    assert_eq!(got, ["319", "311", "303", "295", "287"], "{ty} desc");
    let got = rows(
        &mut e,
        &format!("SELECT id FROM ev WHERE k = {probe} ORDER BY seq ASC LIMIT 5"),
    );
    assert_eq!(got, ["7", "15", "23", "31", "39"], "{ty} asc");
    let all = rows(
        &mut e,
        &format!("SELECT id FROM ev WHERE k = {probe} ORDER BY seq DESC"),
    );
    assert_eq!(
        all.len(),
        40,
        "{ty}: a neighbour leaked in or a row was lost"
    );
    let plan = rows(
        &mut e,
        &format!("EXPLAIN SELECT id FROM ev WHERE k = {probe} ORDER BY seq DESC LIMIT 5"),
    )
    .join("\n");
    assert!(
        plan.contains("Index Scan") && !plan.contains("Sort"),
        "{ty}: the plan still sorts the table —\n{plan}"
    );
}

#[test]
fn numeric_leads_a_composite() {
    walks_in_order("numeric(10,2)", |g| format!("{g}.25"));
}

/// The numeric key is canonical, so the SPELLING of the probe is not
/// part of the question: `7.25`, `7.250` and `07.25` are one value and
/// must reach one group. This is the property `compose_multi_key` could
/// not hold while it keyed by the VALUE — an integer and a decimal
/// spelling of the same number produced two different key variants,
/// which a slice comparison orders by discriminant.
#[test]
fn every_spelling_of_a_numeric_probe_reaches_the_same_group() {
    let mut e = seeded("numeric(10,2)", |g| format!("{g}.25"));
    for spelling in ["7.25", "7.250", "07.25", "7.2500"] {
        let got = rows(
            &mut e,
            &format!("SELECT id FROM ev WHERE k = {spelling} ORDER BY seq DESC LIMIT 3"),
        );
        assert_eq!(got, ["319", "311", "303"], "probe spelled {spelling}");
    }
}

/// A whole number in a numeric column: the probe arrives as an integer
/// and the stored value as a decimal, which is exactly the mismatch.
#[test]
fn an_integer_probe_reaches_a_numeric_group() {
    let mut e = seeded("numeric(10,2)", |g| format!("{g}.00"));
    let got = rows(
        &mut e,
        "SELECT id FROM ev WHERE k = 7 ORDER BY seq DESC LIMIT 3",
    );
    assert_eq!(got, ["319", "311", "303"]);
}

#[test]
fn bytea_leads_a_composite() {
    walks_in_order("bytea", |g| format!("'\\x0{g}'"));
}

#[test]
fn time_leads_a_composite() {
    walks_in_order("time", |g| format!("'0{g}:00:00'"));
}

#[test]
fn timetz_leads_a_composite() {
    walks_in_order("timetz", |g| format!("'0{g}:00:00+00'"));
}

/// `year` is MySQL's — PostgreSQL has no such type — so it is asked on
/// the MySQL wire session, where it is spellable.
#[test]
fn year_leads_a_composite_on_the_mysql_surface() {
    let mut e = Engine::new();
    e.set_mysql_wire_session();
    e.execute("CREATE TABLE ev (id bigint NOT NULL, k year NOT NULL, seq int NOT NULL)")
        .unwrap();
    let mut sql = String::from("INSERT INTO ev VALUES ");
    for g in 0..320u32 {
        if g > 0 {
            sql.push(',');
        }
        sql.push_str(&format!("({}, {}, {})", g, 2000 + (g % 8), g));
    }
    e.execute(&sql).unwrap();
    e.execute("CREATE INDEX ev_k_seq ON ev (k, seq)").unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT id FROM ev WHERE k = 2007 ORDER BY seq DESC LIMIT 5"
        ),
        ["319", "311", "303", "295", "287"]
    );
    let plan = rows(
        &mut e,
        "EXPLAIN SELECT id FROM ev WHERE k = 2007 ORDER BY seq DESC LIMIT 5",
    )
    .join("\n");
    assert!(
        plan.contains("Index Scan") && !plan.contains("Sort"),
        "{plan}"
    );
}

#[test]
fn money_leads_a_composite() {
    walks_in_order("money", |g| format!("'{g}.00'::money"));
}

/// The types already allowed, asked the same way — a regression fence
/// under the rewrite that turned the list into an exhaustive match.
#[test]
fn the_types_that_already_led_a_composite_still_do() {
    walks_in_order("int", |g| format!("{g}"));
    walks_in_order("bigint", |g| format!("{g}"));
    walks_in_order("text", |g| format!("'k{g}'"));
    walks_in_order("varchar(8)", |g| format!("'k{g}'"));
    walks_in_order("date", |g| format!("'2020-01-0{}'", g + 1));
    walks_in_order("timestamp", |g| format!("'2020-01-01 0{g}:00:00'"));
    walks_in_order("timestamptz", |g| format!("'2020-01-01 0{g}:00:00+00'"));
    walks_in_order("uuid", |g| {
        format!("'0000000{g}-0000-0000-0000-000000000000'")
    });
}

/// `bool` has two groups where the helper wants eight, so it gets its
/// own rows: 160 of them under `true`, in order.
#[test]
fn bool_leads_a_composite() {
    let mut e = seeded("bool", |g| {
        String::from(if g % 2 == 0 { "false" } else { "true" })
    });
    let got = rows(
        &mut e,
        "SELECT id FROM ev WHERE k = true ORDER BY seq DESC LIMIT 5",
    );
    assert_eq!(got, ["319", "317", "315", "313", "311"]);
    let all = rows(&mut e, "SELECT id FROM ev WHERE k = true ORDER BY seq DESC");
    assert_eq!(all.len(), 160);
    let plan = rows(
        &mut e,
        "EXPLAIN SELECT id FROM ev WHERE k = true ORDER BY seq DESC LIMIT 5",
    )
    .join("\n");
    assert!(
        plan.contains("Index Scan") && !plan.contains("Sort"),
        "{plan}"
    );
}

/// And a type whose answer is `false` must still give the RIGHT rows —
/// it loses the walk, never the answer. `double precision` is the case
/// that can never be allowed: `f64` is `PartialOrd` and a B-tree cannot
/// hold a key whose comparison may decline to answer.
#[test]
fn a_type_that_cannot_key_still_answers_correctly() {
    let mut e = seeded("double precision", |g| format!("{g}.5"));
    let got = rows(
        &mut e,
        "SELECT id FROM ev WHERE k = 7.5 ORDER BY seq DESC LIMIT 5",
    );
    assert_eq!(got, ["319", "311", "303", "295", "287"]);
}
