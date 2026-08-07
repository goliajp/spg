//! Round 753 (F31 comment audit, tranche 1 #15) — `to_tsvector`
//! positions clamp at 16383, PG18-measured: a 20k-word document's
//! positions top out at 16383 in PG (the MAXENTRYPOS clamp — words
//! keep being recorded, the counter stops), while SPG ran on to
//! 20000. The old comment claimed u16::MAX "matching PG's
//! MaxTSPosition", which was never measured and wrong on both halves.

use spg_engine::{Engine, QueryResult};

#[test]
fn round753_positions_clamp_at_16383_like_pg() {
    let mut e = Engine::new();
    let mut doc = String::new();
    for i in 1..=17000 {
        if i > 1 {
            doc.push(' ');
        }
        doc.push_str(&format!("w{i}"));
    }
    let out = match e
        .execute(&format!("SELECT to_tsvector('simple', '{doc}')::text"))
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    };
    // Positions are the digit runs OUTSIDE the quoted lexemes (which
    // themselves contain digits — 'w17000').
    let mut max_pos = 0u32;
    let mut in_quote = false;
    let mut run = 0u32;
    let mut in_run = false;
    for ch in out.chars() {
        match ch {
            '\'' => in_quote = !in_quote,
            '0'..='9' if !in_quote => {
                run = run * 10 + (ch as u32 - '0' as u32);
                in_run = true;
            }
            _ => {
                if in_run {
                    max_pos = max_pos.max(run);
                    run = 0;
                    in_run = false;
                }
            }
        }
    }
    if in_run {
        max_pos = max_pos.max(run);
    }
    assert_eq!(max_pos, 16383, "positions must clamp at PG's 16383");
    // Every lexeme is still recorded — the clamp stops the counter,
    // not the vector.
    assert!(out.contains("'w17000'"), "late words must still be present");
}
