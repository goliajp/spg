//! v7.39 (round 626, S05b/F29) — a crash, a sum that would not sum, and
//! min/max over types that have no ordering.
//!
//! Extending the coercion probe to the date, JSON and array families found
//! the crash first: `to_char(1, 'YYYY')` **panicked and killed the
//! connection**. Every letter of `YYYY` is a literal in the NUMERIC
//! templates, so the prefix scan claimed all four bytes and the suffix scan
//! claimed all four too, leaving `&pat[4..0]` — "byte range starts at 4 but
//! ends at 0". PG echoes an all-literal pattern verbatim, and so does this
//! now.
//!
//! Then the aggregates, measured type by type against PG18 — eight
//! functions crossed with 33 types, 264 probes:
//!
//!     min/max     PG refuses bool uuid macaddr json jsonb bit varbit xml
//!                 tsvector tsquery; SPG answered for all ten
//!     sum/avg     PG sums a SMALLINT; SPG said "sum/avg need numeric,
//!                 got smallint"
//!     string_agg  PG aggregates a bpchar; SPG refused it
//!
//! `sum` over a SMALLINT column is not an edge case — it is what a query
//! against an ordinary column type looks like, and it errored. The arm was
//! present in three of the four sum accumulators and missing from the
//! fourth. min/max had the mirror-image problem: the guard has to go in all
//! three places the comparison is made, because two of them are inlined
//! copies kept for speed, and those are where a bare `min(TRUE)` lands.
//!
//! The min/max rule is a DENY list of measured rejections, not an allow
//! list. Round 625's first cut of the string-function guard was written as
//! an allow list and refused five overloads PG actually has; a deny list of
//! things PG was measured to refuse cannot over-refuse.
//!
//! Recorded, not closed, and measured: `string_agg` still accepts int2 int4
//! int8 float8 bool oid (its xml arm is load-bearing — xmlagg shares the
//! path — and MySQL's group_concat takes numbers, so this one needs the
//! dialect and the aggregate's name, not just the type); `char_length`
//! accepts bytea bit varbit tsvector; `age` accepts an integer; `sum`/`avg`
//! accept oid and money. Going the other way, PG accepts `sum(time)`,
//! `avg(time)` (both -> interval) and `string_agg(bytea, bytea)` -> bytea,
//! and SPG refuses all three.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
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
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(err) => err.to_string(),
        Ok(ok) => panic!("{sql}: expected a rejection, got {ok:?}"),
    }
}

/// The pattern that panicked, and its neighbours.
#[test]
fn round626_to_char_all_literal_pattern() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT to_char(1,'YYYY'), to_char(1.5,'xyz'), to_char(1,'Q')"),
        vec!["YYYY|xyz|Q"],
        "an all-literal numeric template is echoed, as PG does"
    );
    assert_eq!(vals(&mut e, "SELECT to_char(1,'')"), vec![""]);
    // Still a template where there IS one — `D` is the decimal separator,
    // which is why PG answers ' .AY' here and so does this.
    assert_eq!(vals(&mut e, "SELECT to_char(12,'DAY')"), vec![" .AY"]);
    // And the shape that made it a crash rather than a wrong answer: the
    // connection has to survive.
    assert_eq!(vals(&mut e, "SELECT to_char(1,'YYYY'), 1+1"), vec!["YYYY|2"]);
}

/// sum and avg over a SMALLINT, which is what broke.
#[test]
fn round626_sum_avg_take_smallint() {
    let mut e = Engine::new();
    // PG18 answers exactly this scale for avg over a smallint, measured.
    assert_eq!(
        vals(&mut e, "SELECT sum(1::SMALLINT), avg(1::SMALLINT)"),
        vec!["1|1.00000000000000000000"]
    );
    e.execute("CREATE TABLE q (n SMALLINT)").unwrap();
    e.execute("INSERT INTO q VALUES (1),(2),(3)").unwrap();
    assert_eq!(vals(&mut e, "SELECT sum(n), min(n), max(n), count(n) FROM q"), vec!["6|1|3|3"]);
    assert_eq!(
        vals(&mut e, "SELECT pg_typeof(sum(n)) FROM q"),
        vec!["bigint"],
        "PG sums a smallint to bigint"
    );
    // The wider family still sums.
    assert_eq!(
        vals(&mut e, "SELECT sum(1), sum(1::BIGINT), sum(1.5), sum(1.5::REAL)"),
        vec!["1|1|1.5|1.5"]
    );
}

/// string_agg over a CHAR(n) column.
#[test]
fn round626_string_agg_takes_bpchar() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE c (v CHAR(4))").unwrap();
    e.execute("INSERT INTO c VALUES ('ab'),('cd')").unwrap();
    assert_eq!(
        vals(&mut e, "SELECT string_agg(v, ',') FROM c"),
        vec!["ab,cd"],
        "a bpchar's text form drops its padding, as PG's own cast does"
    );
    assert_eq!(vals(&mut e, "SELECT string_agg('x'::CHAR(2), ',')"), vec!["x"]);
}

/// min/max refuse the types PG has no signature for.
#[test]
fn round626_min_max_reject_unordered_types() {
    let mut e = Engine::new();
    for (sql, ty) in [
        ("SELECT min(TRUE)", "boolean"),
        ("SELECT max(TRUE)", "boolean"),
        ("SELECT min('00000000-0000-0000-0000-000000000000'::UUID)", "uuid"),
        // SPG stores jsonb as its json value, so the message names `json`
        // where PG names `jsonb`. Both refuse; the conflation of the two
        // types is its own item.
        ("SELECT min('{\"a\":1}'::JSONB)", "json"),
        ("SELECT max('{\"a\":1}'::JSON)", "json"),
        ("SELECT min(B'01')", "bit"),
        ("SELECT min('<a/>'::XML)", "xml"),
    ] {
        let m = err(&mut e, sql);
        assert!(
            m.contains("does not exist") && m.contains(ty),
            "{sql}: wanted a rejection naming {ty}, said {m:?}"
        );
    }
    // Through a table, which is where the INLINED accumulators run — the
    // guard had to be repeated there, and a version that only patched the
    // dispatched arm passed the bare-literal cases above while these still
    // answered.
    e.execute("CREATE TABLE b (f BOOL, g INT)").unwrap();
    e.execute("INSERT INTO b VALUES (TRUE,1),(FALSE,1)").unwrap();
    assert!(err(&mut e, "SELECT min(f) FROM b").contains("does not exist"));
    assert!(err(&mut e, "SELECT max(f) FROM b").contains("does not exist"));
    assert!(err(&mut e, "SELECT g, min(f) FROM b GROUP BY g").contains("does not exist"));
}

/// …and still answer for everything PG orders.
#[test]
fn round626_min_max_still_take_what_pg_orders() {
    let mut e = Engine::new();
    assert_eq!(
        vals(
            &mut e,
            "SELECT min(1), max(1::BIGINT), min(1.5), max('x'::TEXT), min(DATE '2020-01-01')"
        ),
        vec!["1|1|1.5|x|2020-01-01"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT max(INTERVAL '1 day'), min(TIME '01:02:03'), \
             max(TIMESTAMP '2020-01-01 00:00:00')"
        ),
        vec!["1 day|01:02:03|2020-01-01 00:00:00"]
    );
    assert_eq!(
        vals(&mut e, "SELECT min('10.0.0.1'::INET), max('\\x41'::BYTEA), min(ARRAY[1])"),
        vec!["10.0.0.1|\\x41|{1}"],
        "inet, bytea and arrays are all ordered in PG"
    );
    assert_eq!(
        vals(&mut e, "SELECT min('x'::CHAR(2)), max('y'::VARCHAR)"),
        // bpchar keeps its padding through min, as PG does.
        vec!["x |y"]
    );
}
