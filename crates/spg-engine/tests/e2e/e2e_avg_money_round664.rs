//! Round 664 — `avg(money)` is a DELIBERATE superset, and this round very
//! nearly deleted it.
//!
//! PG18 has no `avg(money)`; SPG accepts it. F29 enumerated that as one of
//! its "PG refuses, SPG answers" defects, and the guard to refuse it was
//! written and hung on four separate accumulators before a test caught it.
//!
//! The enumeration was not wrong about the fact — PG really does refuse.
//! It was wrong about the CHARACTER of the fact. The decision to accept was
//! deliberate and recorded in three places (`aggregate.rs`'s "we accept it
//! as a sensible superset", plus a doc comment and an inline comment on
//! `e2e_cast_pg_differential::sum_money_agg`), and none of the three is
//! reachable from the ledger line that said "close this".
//!
//! Round 641 had already settled the policy for exactly this situation:
//! judge each divergence by correctness risk, not by divergence itself. It
//! kept `count(DISTINCT xid)` ("a superset with no correctness risk, so it
//! stays") and separately recorded that `array_agg(x ORDER BY x)` over xid
//! SHOULD refuse, because that one genuinely needs an ordering. Measured
//! against that policy, `avg(money)` stays: money is a cents type, so a
//! rounded average is what the type can represent rather than a loss the
//! operation introduced, and no PG application can reach the shape at all,
//! since PG refuses it.
//!
//! So this file pins the superset instead of the refusal. The values are
//! SPG's own — there is no PG oracle for an expression PG rejects — and
//! every one below was measured on the wire before being written down.
//!
//! Only ONE shape (`avg(m)` with no GROUP BY) was pinned before today, and
//! the deleted guard sailed past it at three of its four sites. The other
//! shapes here are the ones that were undefended.

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
    e.execute("CREATE TABLE ag(m MONEY, g INT)").unwrap();
    e.execute("INSERT INTO ag VALUES (5,1),(6,1),(7,2)").unwrap();
}

#[test]
fn round664_avg_money_answers_at_every_shape() {
    let mut e = Engine::new();
    seed(&mut e);

    // A — literal argument, folded before any accumulator runs.
    assert_eq!(one(&mut e, "SELECT avg('$1'::money)"), "$1.00");
    // B — column, no GROUP BY. The only shape pinned before today.
    assert_eq!(one(&mut e, "SELECT avg(m) FROM ag"), "$6.00");
    // C — grouped, a different accumulator from B.
    assert_eq!(
        one(&mut e, "SELECT g, avg(m) FROM ag GROUP BY g ORDER BY g"),
        "1|$5.50,2|$7.00"
    );
    // D — FILTER.
    assert_eq!(
        one(&mut e, "SELECT avg(m) FILTER (WHERE g=1) FROM ag"),
        "$5.50"
    );
    // E — DISTINCT.
    assert_eq!(one(&mut e, "SELECT avg(DISTINCT m) FROM ag"), "$6.00");
    // S — fused beside sum, which shares one accumulator slot with avg.
    // Both answers have to survive the sharing.
    assert_eq!(one(&mut e, "SELECT sum(m), avg(m) FROM ag"), "$18.00|$6.00");
}

#[test]
fn round664_avg_money_rounds_half_away_from_zero_both_ways() {
    // The rounding is the whole reason PG declined to define this, so it is
    // the part most worth holding still. Symmetric about zero: a -0.5c
    // average must not round toward zero just because it is negative.
    let mut e = Engine::new();
    e.execute("CREATE TABLE agp(m MONEY)").unwrap();
    e.execute("INSERT INTO agp VALUES (0.05),(0.06)").unwrap();
    assert_eq!(one(&mut e, "SELECT avg(m) FROM agp"), "$0.06");

    e.execute("CREATE TABLE agn(m MONEY)").unwrap();
    e.execute("INSERT INTO agn VALUES (-0.05),(-0.06)").unwrap();
    assert_eq!(one(&mut e, "SELECT avg(m) FROM agn"), "-$0.06");
}

#[test]
fn round664_avg_money_keeps_ordinary_aggregate_semantics() {
    // Being a superset does not license inventing behaviour elsewhere: an
    // empty group is still NULL, and NULLs are still skipped rather than
    // counted as zero (counting them would give $2.00 below).
    let mut e = Engine::new();
    seed(&mut e);

    assert_eq!(one(&mut e, "SELECT avg(m) FROM ag WHERE g=99"), "NULL");
    assert_eq!(
        one(
            &mut e,
            "SELECT avg(m) FROM (SELECT NULL::money AS m UNION ALL SELECT 4::money) z"
        ),
        "$4.00"
    );
}

#[test]
fn round664_sum_money_is_untouched() {
    let mut e = Engine::new();
    seed(&mut e);
    assert_eq!(one(&mut e, "SELECT sum(m) FROM ag"), "$18.00");
    assert_eq!(
        one(&mut e, "SELECT g, sum(m) FROM ag GROUP BY g ORDER BY g"),
        "1|$11.00,2|$7.00"
    );
    // And avg over an ordinary numeric column is unaffected.
    assert_eq!(one(&mut e, "SELECT avg(g) FROM ag"), "1.3333333333333333");
}
