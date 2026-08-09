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
    // `locate_column` declines to bind it and the general path answers
    // with the composite of the whole row.
    //
    // Round 957 found this ERRORING while PG18 answered `(7,z)`, and
    // round 961 closed it: the gap was a THIRD resolver,
    // `resolve_projection_column`, which types the projection before any
    // row is read and had no whole-row branch, so it raised
    // ColumnNotFound before `resolve_column`'s branch (round T9) could
    // run. Same class as round 823 — two resolvers for one question, one
    // of them missing a rule.
    let rows = streamed(&e, "SELECT wr FROM wr").expect("whole-row reference");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(
        rows[0].contains('7') && rows[0].contains('z'),
        "the whole row, not one of its columns: {rows:?}"
    );

    // A real column named like the alias still wins, which is the
    // precedence the evaluation side already applied.
    let mut e2 = Engine::new();
    run(&mut e2, "CREATE TABLE s (s INT, other TEXT)");
    run(&mut e2, "INSERT INTO s VALUES (5, 'q')");
    let shadow = streamed(&e2, "SELECT s FROM s").expect("column shadows the alias");
    assert_eq!(shadow, vec!["Int(5)".to_string()], "{shadow:?}");
}

#[test]
fn whole_row_reference_works_in_a_join_too() {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE wr (id INT, pad TEXT)");
    run(&mut e, "INSERT INTO wr VALUES (7, 'z')");
    run(&mut e, "CREATE TABLE jb (id INT, w TEXT)");
    run(&mut e, "INSERT INTO jb VALUES (7, 'J')");

    // A join's combined schema carries no alias and qualifies every
    // column `alias.col`, so both the typing side and the evaluation
    // side identify the alias by that prefix. Verified against PG18.4
    // over the wire, round 961: each of these answers identically there.
    let both = streamed(&e, "SELECT wr, jb FROM wr JOIN jb ON wr.id = jb.id").expect("join");
    assert_eq!(both.len(), 1, "{both:?}");
    assert!(
        both[0].contains('z') && both[0].contains('J'),
        "each side's whole row, side by side: {both:?}"
    );

    let aliased =
        streamed(&e, "SELECT a FROM wr a JOIN jb b ON a.id = b.id").expect("aliased join");
    assert!(aliased[0].contains('z'), "{aliased:?}");

    // A name that is neither a column nor an alias still errors.
    assert!(
        streamed(&e, "SELECT nosuch FROM wr JOIN jb ON wr.id = jb.id").is_err(),
        "a name matching no alias must not become a whole-row reference"
    );
}

#[test]
fn a_null_extended_side_is_a_composite_of_nulls_not_null() {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE wr (id INT, pad TEXT)");
    run(&mut e, "INSERT INTO wr VALUES (7, 'z')");
    run(&mut e, "CREATE TABLE jb (id INT, w TEXT)");
    run(&mut e, "INSERT INTO jb VALUES (7, 'J')");

    // KNOWN GAP, pinned so it is not mistaken for correct. PG18.4
    // answers a whole-row reference to the UNMATCHED side of an outer
    // join with NULL; SPG answers `(,)` — a composite whose fields are
    // all NULL. Measured on both, round 961.
    //
    // The two are only distinguishable with information SPG's combined
    // row does not carry: which sides were null-extended. One streaming
    // join walk does know it (`tuple[k] == usize::MAX`, `select.rs`) but
    // decomposes the projection per column, so a whole-row item never
    // sees it; the materialising walk fills NULLs with no marker at all.
    // Closing it means recording null-extension on the combined row,
    // which is why it is not closed here.
    //
    // Distinguishing by "all fields are NULL" would be a guess, not a
    // fix: a real row whose every column is NULL is `(,)` in PG too.
    let rows = streamed(&e, "SELECT jb FROM wr LEFT JOIN jb ON wr.id = jb.id + 99")
        .expect("left join, no match");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(
        rows[0].contains("Composite"),
        "today: a composite of NULLs; PG18.4: NULL — got {rows:?}"
    );

    // The matched side is right, and that is what makes the gap narrow.
    let kept = streamed(&e, "SELECT wr FROM wr LEFT JOIN jb ON wr.id = jb.id + 99")
        .expect("left join, kept side");
    assert!(kept[0].contains('z'), "{kept:?}");
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
