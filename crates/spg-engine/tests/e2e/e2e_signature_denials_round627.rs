//! v7.39 (round 627, S05b/F29) — `char_length` is not `length`, and `age`
//! does not take a number.
//!
//! Both share code with something wider than their own signature, and both
//! answered where PG refuses. Measured across the length family and the
//! temporal family:
//!
//!     char_length(bytea)      PG refuses   SPG answered
//!     char_length(B'01')      PG refuses   SPG answered
//!     char_length(varbit)     PG refuses   SPG answered
//!     char_length(tsvector)   PG refuses   SPG answered
//!
//! `age` looked like the same shape and is NOT, and the reason is worth
//! recording: PG refuses `age(1)` because its wraparound overload is
//! `age(xid)`, and the canonical query `SELECT age(relfrozenxid) FROM
//! pg_class` type-checks there because PG declares that column `xid`. SPG
//! declares it `bigint`, so a guard that refused an integer would refuse
//! the very query the overload exists for. The guard was written, it broke
//! exactly that, and it is reverted — the fix is to type the column as xid
//! in the catalog, which belongs with the catalog work, not here.
//!
//! `length` takes every one of those in PG — it counts octets or bits — so
//! the refusal is on the SPELLING, not the shared arm. Both rules are deny
//! lists of what PG was measured to refuse, which is what keeps the
//! character types `char_length` does take out of their way.
//!
//! Recorded, not closed: `age(1)`, `age('x')` are still accepted (see
//! above); `sum(oid)`, `avg(oid)` and `avg(money)` are still
//! accepted where PG refuses. They live in the aggregate accumulators —
//! four copies of them — and OID may not be distinguishable from INT at the
//! value level, so they need the accumulator work (F32) first.

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

#[test]
fn round627_char_length_refuses_what_length_takes() {
    let mut e = Engine::new();
    for sql in [
        "SELECT char_length('\\x41'::BYTEA)",
        "SELECT char_length(B'01')",
        "SELECT char_length(B'01'::VARBIT)",
        "SELECT char_length('a'::TSVECTOR)",
        "SELECT character_length('\\x41'::BYTEA)",
    ] {
        let m = err(&mut e, sql);
        assert!(m.contains("does not exist"), "{sql}: {m}");
    }
    // …while `length` takes all of them, as PG does.
    assert_eq!(
        vals(
            &mut e,
            "SELECT length('\\x41'::BYTEA), length(B'01'), length(B'01'::VARBIT)"
        ),
        vec!["1|2|2"]
    );
    // And char_length still measures the character types.
    assert_eq!(
        vals(
            &mut e,
            "SELECT char_length('abc'), char_length('ab'::CHAR(4)), character_length('xy'::VARCHAR)"
        ),
        vec!["3|2|2"]
    );
}

