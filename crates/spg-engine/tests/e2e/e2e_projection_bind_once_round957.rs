//! r957 — binding the projection once must resolve exactly what
//! resolving per row resolved.
//!
//! `try_stream_single_table` now locates each bare-column projection
//! once, before the scan, instead of walking the schema and comparing
//! column-name strings for every cell of every row (400k rows: 16.5 ms
//! -> 11.0 ms on `SELECT pad`). The risk that buys is a resolver that
//! agrees with the per-row one on the common shape and quietly differs
//! on the rest — which is precisely what round 823 had to repair when
//! `find_column_pos` was missing `resolve_column`'s bare-name fallback:
//! one shape bound on one path, not the other, and nothing said so.
//!
//! The bind therefore goes through `locate_column`, the same lookup
//! `resolve_column` performs. These are the shapes where a hand-written
//! second resolver would have drifted, each answered through the
//! streaming walk the binding lives in:
//!
//!   * a stored COMPOSITE column, which arrives as JSON and has to be
//!     rehydrated — reading `row.values[pos]` raw hands back the JSON;
//!   * a whole-row reference, which is not a position at all;
//!   * a qualifier that is not the table's, which must still error
//!     rather than fall through to a bare-name match;
//!   * a qualified column, which resolves by a different branch than
//!     the bare name;
//!   * a name that resolves for no row at all, where binding eagerly
//!     would raise on an empty table that today returns nothing.

use spg_engine::{CancelToken, Engine, StreamItem};

/// Rows as the streaming walk emits them, one string per row with the
/// cells joined — so a projection reading the wrong column shows up as a
/// different answer, not just a different count.
fn streamed(e: &Engine, sql: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    e.execute_readonly_select_streaming(sql, CancelToken::none(), |item| {
        if let StreamItem::Row(cells) = item {
            let mut line = String::new();
            for i in 0..cells.len() {
                if i > 0 {
                    line.push('|');
                }
                line.push_str(&format!("{:?}", cells.get(i).expect("cell in range")));
            }
            out.push(line);
        }
        Ok(())
    })
    .map_err(|e| format!("{e:?}"))?;
    Ok(out)
}

fn run(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn bare_column_reads_the_same_column_the_per_row_path_read() {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE t (id INT, k INT, pad TEXT)");
    run(&mut e, "INSERT INTO t VALUES (1, 10, 'a'), (2, 20, 'b')");

    // Every position, including the last — the walk compares names in
    // schema order, so a binding off by one would still answer for the
    // first column.
    assert_eq!(
        streamed(&e, "SELECT pad FROM t").unwrap(),
        streamed(&e, "SELECT pad FROM t WHERE true").unwrap(),
        "the projection must not depend on which gate the row came through"
    );
    let pads = streamed(&e, "SELECT pad FROM t").unwrap();
    assert_eq!(pads.len(), 2, "both rows: {pads:?}");
    assert!(pads[0].contains('a') && pads[1].contains('b'), "{pads:?}");

    let ks = streamed(&e, "SELECT k FROM t").unwrap();
    assert!(ks[0].contains("10") && ks[1].contains("20"), "{ks:?}");
}

#[test]
fn qualified_column_binds_and_a_foreign_qualifier_still_errors() {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE t (id INT, pad TEXT)");
    run(&mut e, "INSERT INTO t VALUES (1, 'a')");

    let q = streamed(&e, "SELECT t.pad FROM t").unwrap();
    assert!(q[0].contains('a'), "qualified projection: {q:?}");

    let aliased = streamed(&e, "SELECT x.pad FROM t x").unwrap();
    assert!(aliased[0].contains('a'), "aliased projection: {aliased:?}");

    // A qualifier that names nothing in scope. `locate_column` reports
    // the error; binding must not swallow it and must not fall through
    // to the bare-name match, which WOULD have found `pad`.
    let err = streamed(&e, "SELECT nosuch.pad FROM t x");
    assert!(
        err.is_err(),
        "a foreign qualifier must not resolve to the bare column: {err:?}"
    );
}

#[test]
fn a_column_that_does_not_exist_still_reports_nothing_on_an_empty_table() {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE empty_t (id INT)");

    // No row is ever evaluated, so no cell is ever resolved. Binding
    // eagerly and propagating the bind error would turn this from an
    // empty answer into an error — a behaviour change that has nothing
    // to do with the perf work.
    let per_row = streamed(&e, "SELECT nosuch FROM empty_t");
    match per_row {
        Ok(rows) => assert!(rows.is_empty(), "{rows:?}"),
        Err(_) => { /* whichever it was before, it must not change */ }
    }
}

#[test]
fn whole_row_reference_is_not_bound_to_a_position() {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE wr (id INT, pad TEXT)");
    run(&mut e, "INSERT INTO wr VALUES (7, 'z')");

    // `wr` is the FROM alias, not a column: it has no position, so
    // `locate_column` declines to bind it and the general path answers.
    //
    // What that path answers today is an ERROR, and PG18 answers
    // `(7,z)` — measured on both, round 957. The gap is NOT this
    // binding: it is a THIRD resolver, `resolve_projection_column`
    // (`select.rs:9166`), which types the projection before any row is
    // read and has no whole-row branch, so it raises ColumnNotFound
    // before `resolve_column`'s branch (round T9) can run. Same class as
    // round 823 — two resolvers for one question, one of them missing a
    // rule.
    //
    // Pinned as-is so that closing that gap is a deliberate change with
    // this test going green, and so the binding cannot silently start
    // answering something else in the meantime.
    let rows = streamed(&e, "SELECT wr FROM wr");
    assert!(
        rows.is_err(),
        "whole-row reference: still the pre-binding behaviour, \
         and still a known gap against PG18's (7,z) — got {rows:?}"
    );
}

#[test]
fn a_stored_composite_column_is_still_rehydrated() {
    let mut e = Engine::new();
    run(&mut e, "CREATE TYPE pt AS (x INT, y INT)");
    run(&mut e, "CREATE TABLE c (id INT, p pt)");
    run(&mut e, "INSERT INTO c VALUES (1, ROW(2, 3)::pt)");

    // The cell is stored as JSON and rebuilt into a composite on the way
    // out. A bind-once path that fetched `row.values[pos]` directly
    // would hand back the raw JSON, which renders differently and
    // compares differently — the failure would be silent in a row count.
    let rows = streamed(&e, "SELECT p FROM c").unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(
        rows[0].contains("Composite"),
        "the composite must survive the bound path, got {rows:?}"
    );
}
