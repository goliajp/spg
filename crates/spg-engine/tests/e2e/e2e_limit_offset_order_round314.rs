//! v7.39 (round 314, V39) — the row-count clauses come in either order.
//!
//! PG's grammar takes a limit clause and an offset clause as an
//! UNORDERED pair, so `OFFSET 2 LIMIT 3` means exactly what
//! `LIMIT 3 OFFSET 2` does. This parser read them in a fixed
//! LIMIT-then-OFFSET sequence, so the other spelling — the one a lot of
//! hand-written SQL and some ORMs emit — died on `expected end of input,
//! got Limit`.
//!
//! The same is true of `FETCH FIRST`, which is the standard's spelling
//! of the same clause: it may come before or after OFFSET. What PG does
//! NOT allow is two of either, or one of each spelling together, and
//! those rejections are pinned too — accepting them would mean silently
//! honouring one clause and dropping the other.
//!
//! Every expectation read off live PG 18.4 (2026-07-21).

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE g39 (g int)").unwrap();
    e.execute("INSERT INTO g39 VALUES (1),(2),(3),(4),(5),(6),(7),(8),(9),(10)")
        .unwrap();
    e
}

/// Five spellings of the same window; PG answers 3,4,5 to all of them.
#[test]
fn either_order_selects_the_same_window() {
    let mut e = fixture();
    for sql in [
        "SELECT g FROM g39 ORDER BY g LIMIT 3 OFFSET 2",
        "SELECT g FROM g39 ORDER BY g OFFSET 2 LIMIT 3",
        "SELECT g FROM g39 ORDER BY g OFFSET 2 FETCH FIRST 3 ROWS ONLY",
        "SELECT g FROM g39 ORDER BY g FETCH FIRST 3 ROWS ONLY OFFSET 2",
        "SELECT g FROM g39 ORDER BY g OFFSET 2 ROWS FETCH FIRST 3 ROWS ONLY",
    ] {
        assert_eq!(rows(&mut e, sql), ["3", "4", "5"], "{sql}");
    }
}

/// Each clause at most once, and LIMIT / FETCH FIRST are one clause in
/// two spellings. Taking a second one would mean quietly honouring
/// whichever the parser happened to read last.
#[test]
fn a_repeated_or_doubled_clause_is_refused() {
    let mut e = fixture();
    for sql in [
        "SELECT g FROM g39 LIMIT 1 LIMIT 2",
        "SELECT g FROM g39 LIMIT 1 OFFSET 2 LIMIT 3",
        "SELECT g FROM g39 OFFSET 1 OFFSET 2",
        "SELECT g FROM g39 LIMIT 2 FETCH FIRST 3 ROWS ONLY",
        // ORDER BY still has to come first.
        "SELECT g FROM g39 OFFSET 2 ORDER BY g",
    ] {
        assert!(e.execute(sql).is_err(), "{sql} must not parse");
    }
}

/// The single-clause and dialect forms this restructuring could have
/// broken: a lone LIMIT or OFFSET, the unbounded sentinels, MySQL's
/// `LIMIT offset, count`, and a bare FETCH FIRST.
#[test]
fn the_existing_spellings_are_unchanged() {
    let mut e = fixture();
    assert_eq!(rows(&mut e, "SELECT g FROM g39 ORDER BY g LIMIT 2"), ["1", "2"]);
    assert_eq!(rows(&mut e, "SELECT g FROM g39 ORDER BY g OFFSET 8"), ["9", "10"]);
    assert_eq!(
        rows(&mut e, "SELECT g FROM g39 ORDER BY g LIMIT ALL OFFSET 8"),
        ["9", "10"]
    );
    // MySQL's two-argument form: the FIRST number is the offset.
    assert_eq!(
        rows(&mut e, "SELECT g FROM g39 ORDER BY g LIMIT 2, 3"),
        ["3", "4", "5"]
    );
    assert_eq!(
        rows(&mut e, "SELECT g FROM g39 ORDER BY g FETCH FIRST 2 ROWS ONLY"),
        ["1", "2"]
    );
    // WITH TIES rides on the same clause and still applies.
    assert_eq!(
        rows(
            &mut e,
            "SELECT g FROM g39 ORDER BY g FETCH FIRST 2 ROWS WITH TIES"
        ),
        ["1", "2"]
    );
}
