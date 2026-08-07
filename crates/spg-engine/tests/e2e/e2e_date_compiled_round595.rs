//! v7.39 (round 595) — one date function kept the whole WHERE off the
//! compiled path.
//!
//! Round 594 asked for the ledger's other `count(*)`-wrapped ratios to be
//! re-checked for the operator elision that invalidated the window numbers.
//! They hold: PG really sorts, really scans with the filter, really hash
//! joins, really top-N heapsorts. So the list stood, and re-measuring it put
//! the date functions at the top — with `date_trunc` worse than the
//! `extract` the ledger had recorded. 500k rows:
//!
//!     date_trunc('day', t) = TIMESTAMP …  153.82 -> 17.87 ms   PG  9.69
//!     extract(month FROM t) = 1            81.76 -> 11.12      PG 13.59
//!     extract(year FROM t) = 2020          81.73 -> 11.63      PG 14.52
//!     to_char(t, 'YYYY') = '2020'         104.19 -> 13.85      PG 20.46
//!     extract(epoch FROM t) > 0            69.56 ->  8.61      PG 15.67
//!
//! Four of those five now win. What made them slow was not the function: a
//! compiled comparison on the same column costs 13.1 ms, and `lower(s) =
//! 'row1'` — the same shape with a whitelisted function — was already 11.5.
//! The predicate compiler is all-or-nothing, so ONE node it does not know
//! disqualifies the whole WHERE and the column read and the comparison get
//! interpreted per row too.
//!
//! `EXTRACT` was excluded because its field is a keyword parsed off the tree
//! rather than an argument — so the field rides in the step and the source is
//! the preceding sub-program. `date_trunc` / `to_char` / `date_part` / `age`
//! were excluded as not context-free, which is true and turns out not to
//! matter: they are fixed for the whole statement, reading the session's zone
//! and DateStyle out of the `EvalContext`, and `Step::Function` already hands
//! that context to the same implementation the interpreter calls. `now` and
//! `random` stay off — they are not fixed in that way.
//!
//! What the pins are for. The compiled step has to answer exactly what the
//! interpreter answers, over every field, every source type, NULLs, and the
//! session-dependent renderings. All 16 shapes here were checked against live
//! PG18; 15 matched byte for byte, and the one that did not is the error
//! WORDING for `extract(year FROM <text>)`, which is unchanged by this round
//! (verified by running the same file against the previous binary) and is
//! recorded in the ledger as its own gap: PG rejects that call by resolution,
//! SPG accepts it and errors at runtime.

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

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE dt (id INT, ts TIMESTAMP, d DATE, tm TIME, iv INTERVAL, s TEXT)")
        .unwrap();
    e.execute(
        "INSERT INTO dt VALUES \
         (1,'2020-01-02 03:04:05','2020-01-02','03:04:05','1 day 2 hours','x'),\
         (2,'1999-12-31 23:59:59','1999-12-31','23:59:59','3 months','y'),\
         (3,NULL,NULL,NULL,NULL,NULL),\
         (4,'2024-02-29 12:00:00','2024-02-29','00:00:00','-1 day','z')",
    )
    .unwrap();
    e
}

/// Every field, off a timestamp, through the compiled step.
#[test]
fn round595_extract_fields() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, extract(year FROM ts), extract(month FROM ts), extract(day FROM ts), \
             extract(hour FROM ts), extract(minute FROM ts), extract(second FROM ts) \
             FROM dt ORDER BY id"
        ),
        vec![
            "1|2020|1|2|3|4|5.000000",
            "2|1999|12|31|23|59|59.000000",
            "3|NULL|NULL|NULL|NULL|NULL|NULL",
            "4|2024|2|29|12|0|0.000000",
        ]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, extract(dow FROM ts), extract(doy FROM ts), extract(quarter FROM ts), \
             extract(week FROM ts), extract(epoch FROM ts) FROM dt ORDER BY id"
        ),
        vec![
            "1|4|2|1|1|1577934245.000000",
            "2|5|365|4|52|946684799.000000",
            "3|NULL|NULL|NULL|NULL|NULL",
            "4|4|60|1|9|1709208000.000000",
        ]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, extract(year FROM d), extract(hour FROM tm), extract(day FROM iv), \
             extract(month FROM iv) FROM dt ORDER BY id"
        ),
        vec![
            "1|2020|3|1|0",
            "2|1999|23|0|3",
            "3|NULL|NULL|NULL|NULL",
            "4|2024|0|-1|0"
        ],
        "date, time and interval sources"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, extract(year FROM ts::DATE), extract(year FROM d::TIMESTAMP) \
             FROM dt ORDER BY id"
        ),
        vec!["1|2020|2020", "2|1999|1999", "3|NULL|NULL", "4|2024|2024"],
        "a cast source, which is itself compiled"
    );
    assert_eq!(
        vals(&mut e, "SELECT extract(year FROM NULL::TIMESTAMP)"),
        vec!["NULL"]
    );
}

/// The functions that read the session out of the context, now compiled.
#[test]
fn round595_session_deterministic_functions() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, date_trunc('year', ts), date_trunc('month', ts), \
             date_trunc('day', ts), date_trunc('hour', ts) FROM dt ORDER BY id"
        ),
        vec![
            "1|2020-01-01 00:00:00|2020-01-01 00:00:00|2020-01-02 00:00:00|2020-01-02 03:00:00",
            "2|1999-01-01 00:00:00|1999-12-01 00:00:00|1999-12-31 00:00:00|1999-12-31 23:00:00",
            "3|NULL|NULL|NULL|NULL",
            "4|2024-01-01 00:00:00|2024-02-01 00:00:00|2024-02-29 00:00:00|2024-02-29 12:00:00",
        ]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, to_char(ts,'YYYY'), to_char(ts,'YYYY-MM-DD'), to_char(ts,'HH24:MI:SS'), \
             to_char(ts,'Day') FROM dt ORDER BY id"
        ),
        vec![
            "1|2020|2020-01-02|03:04:05|Thursday ",
            "2|1999|1999-12-31|23:59:59|Friday   ",
            "3|NULL|NULL|NULL|NULL",
            "4|2024|2024-02-29|12:00:00|Thursday ",
        ],
        "including the blank-padded day name"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, date_part('year', ts), date_part('epoch', ts) FROM dt ORDER BY id"
        ),
        vec![
            "1|2020|1577934245",
            "2|1999|946684799",
            "3|NULL|NULL",
            "4|2024|1709208000"
        ]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, age(TIMESTAMP '2020-01-02 03:04:05', ts) FROM dt ORDER BY id"
        ),
        vec![
            "1|00:00:00",
            "2|20 years 1 day 03:04:06",
            "3|NULL",
            "4|-4 years -1 mons -27 days -08:55:55",
        ]
    );
}

/// In a filter is where the all-or-nothing gate mattered: the comparison and
/// the column read were interpreted too.
#[test]
fn round595_filters_and_composition() {
    let mut e = seed();
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM dt WHERE extract(year FROM ts) = 2020 ORDER BY id"
        ),
        vec!["1"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM dt WHERE extract(month FROM ts) * 2 = 24 ORDER BY id"
        ),
        vec!["2"],
        "the extracted value feeding arithmetic"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM dt WHERE date_trunc('day', ts) = TIMESTAMP '2020-01-02 00:00:00' \
             ORDER BY id"
        ),
        vec!["1"]
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id FROM dt WHERE extract(year FROM ts) >= 2000 \
             AND date_trunc('year', ts) <= TIMESTAMP '2024-01-01 00:00:00' \
             AND to_char(ts,'YYYY') <> '1999' ORDER BY id"
        ),
        vec!["1", "4"],
        "three of the family in one predicate"
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT id, CASE WHEN extract(year FROM ts) > 2000 THEN date_trunc('month', ts) \
             ELSE NULL END FROM dt ORDER BY id"
        ),
        vec![
            "1|2020-01-01 00:00:00",
            "2|NULL",
            "3|NULL",
            "4|2024-02-01 00:00:00",
        ],
        "nested inside a CASE, which is itself compiled"
    );
}

/// At a size where the per-row interpretation was the whole cost, the
/// compiled answer has to be the interpreted one.
#[test]
fn round595_scale_agrees() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE big (id INT, t TIMESTAMP)").unwrap();
    e.execute(
        "INSERT INTO big SELECT gg, TIMESTAMP '2020-01-01 00:00:00' + (gg * INTERVAL '1 hour') \
         FROM generate_series(1, 5000) gg",
    )
    .unwrap();
    // 5000 hours from 2020-01-01 lands in 2020 for the first 8784 (leap
    // year), so every row is 2020 — checked against the row count rather
    // than by hand.
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM big WHERE extract(year FROM t) = 2020"
        ),
        vals(&mut e, "SELECT count(*) FROM big")
    );
    // The month boundaries have to agree with a plain comparison.
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM big WHERE date_trunc('month', t) = TIMESTAMP '2020-01-01 00:00:00'"
        ),
        vals(
            &mut e,
            "SELECT count(*) FROM big WHERE t >= TIMESTAMP '2020-01-01 00:00:00' \
             AND t < TIMESTAMP '2020-02-01 00:00:00'"
        )
    );
    assert_eq!(
        vals(
            &mut e,
            "SELECT count(*) FROM big WHERE to_char(t, 'YYYY') = '2020'"
        ),
        vals(
            &mut e,
            "SELECT count(*) FROM big WHERE extract(year FROM t) = 2020"
        )
    );
}
