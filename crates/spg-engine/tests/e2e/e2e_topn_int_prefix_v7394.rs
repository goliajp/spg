//! v7.39.4 — the top-N boundary gate, now that an INTEGER key can reach
//! it.
//!
//! `ORDER BY <int> LIMIT 10` over 400,000 rows built 400,000 sort keys
//! to keep ten: the gate that turns a decisively losing row away before
//! its key is built (v7.38.20) read text and nothing else, so an
//! integer column took the ordinary path. Extending it is a REJECTION
//! path, and a rejection path that is wrong drops rows silently, so
//! these pin the shapes where an eight-byte summary of an `i128` could
//! disagree with the key it summarises: the sign boundary, ties on the
//! boundary, NULLs, values wider than the prefix, and a column whose
//! rows are not all the same kind.
//!
//! Three things about the fixtures, each learned from an ablation that
//! did NOT bite:
//!
//!  * The ORDER BY key must NOT be in the select list. The gate stands
//!    in the branch that builds keys from the ROW, and a query whose
//!    ordering names an output column sorts by the projection instead
//!    (`sort_by_output`), which skips it entirely. Every query here
//!    projects `tag` and orders by something else.
//!  * The table must exceed `TRIM_FLOOR` = 1024 rows, because the
//!    boundary the gate reads is set by the accumulator's first trim.
//!  * The padding must be inserted BEFORE the values under test. Values
//!    inserted first are already in the accumulator when the boundary
//!    appears, so the gate only ever sees padding it is right to reject
//!    and the answer survives any encoding at all.
//!
//! With all three, breaking the encoding reddens these; with any one of
//! them missing, summarising every integer row as `u64::MAX` left them
//! green.

use spg_engine::{Engine, QueryResult};

const PAD_ROWS: i64 = 4096;

fn tags(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows");
    };
    rows.iter()
        .map(|r| match &r.values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            spg_storage::Value::Null => "<null>".to_string(),
            other => panic!("{sql}: expected text, got {other:?}"),
        })
        .collect()
}

/// `k` carries the value under test; `tag` spells it, so the answer can
/// be read without putting `k` in the select list.
fn seeded(decl: &str, padding: &str, values: &[&str]) -> Engine {
    let mut e = Engine::new();
    e.execute(&format!("CREATE TABLE t (k {decl}, tag TEXT)"))
        .unwrap();
    e.execute(&format!(
        "INSERT INTO t SELECT {padding}, 'pad' FROM generate_series(1, {PAD_ROWS}) g"
    ))
    .unwrap();
    for v in values {
        // The tag spells the value; a quoted literal loses its quotes so
        // the tag is a plain string either way.
        let tag = v.trim_matches('\'');
        e.execute(&format!("INSERT INTO t VALUES ({v}, '{tag}')"))
            .unwrap();
    }
    e
}

#[test]
fn negatives_are_not_ordered_after_positives() {
    // The prefix is a biased big-endian encoding; an unbiased one puts
    // every negative above every positive, and the gate then rejects
    // exactly the rows that belong in the answer.
    let mut e = seeded(
        "BIGINT",
        "1000000 + g",
        &["5", "-1", "3", "-100", "0", "2", "-7", "9"],
    );
    assert_eq!(
        tags(&mut e, "SELECT tag FROM t ORDER BY k LIMIT 4"),
        vec!["-100", "-7", "-1", "0"]
    );
}

#[test]
fn a_tie_on_the_boundary_keeps_the_full_limit() {
    // The gate rejects only on a STRICT difference; a row equal to the
    // boundary has to survive to the ordinary comparison.
    let mut e = seeded("BIGINT", "1000000 + g", &["1", "1", "1", "1", "2", "2"]);
    let got = tags(&mut e, "SELECT tag FROM t ORDER BY k LIMIT 6");
    assert_eq!(got, vec!["1", "1", "1", "1", "2", "2"]);
}

#[test]
fn values_wider_than_the_prefix_still_order() {
    // Beyond `i64` the prefix saturates, so two such values summarise
    // identically and must fall through to the exact key.
    let mut e = seeded(
        "NUMERIC",
        "9300000000000000000 + g",
        &[
            "9223372036854775807",
            "-9223372036854775808",
            "1",
            "0",
            "-1",
        ],
    );
    assert_eq!(
        tags(&mut e, "SELECT tag FROM t ORDER BY k LIMIT 3"),
        vec!["-9223372036854775808", "-1", "0"]
    );
}

#[test]
fn a_null_never_displaces_a_real_value() {
    // A NULL has no prefix, so the gate has to let it through to the
    // ordinary comparison rather than summarising it as zero — under
    // ASC it sorts last.
    let mut e = seeded("INT", "1000000 + g", &["3", "NULL", "1", "NULL", "2"]);
    assert_eq!(
        tags(&mut e, "SELECT tag FROM t ORDER BY k LIMIT 3"),
        vec!["1", "2", "3"]
    );
}

#[test]
fn a_date_key_reaches_the_same_gate() {
    // `Date` keys as an integer too, and its prefix must agree with it
    // — including before the epoch, where the day count is negative.
    let mut e = seeded(
        "DATE",
        "DATE '2100-01-01' + g",
        &[
            "'1999-12-31'",
            "'2026-08-31'",
            "'1970-01-01'",
            "'1969-12-31'",
        ],
    );
    assert_eq!(
        tags(&mut e, "SELECT tag FROM t ORDER BY k LIMIT 2"),
        vec!["1969-12-31", "1970-01-01"]
    );
}

#[test]
fn a_text_key_still_orders_by_its_bytes() {
    // The other half of the pair, unchanged: a text prefix and an
    // integer prefix are both eight bytes and mean nothing to each
    // other, so the gate compares them only when they say the same
    // kind.
    let mut e = seeded("TEXT", "'zz' || g", &["'10'", "'9'", "'100'", "'2'"]);
    assert_eq!(
        tags(&mut e, "SELECT tag FROM t ORDER BY k LIMIT 3"),
        vec!["10", "100", "2"]
    );
}
