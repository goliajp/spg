//! r970 — the single-table streaming walk asks the indices before it
//! walks the table.
//!
//! It never did, and it is preferred over the materialising path, which
//! does. So a primary-key point lookup read every row: measured on 500k
//! rows, `SELECT * FROM big WHERE id = 250000` took 14.9 ms against
//! PG18.4's 0.17 ms, and the cost tracked the TABLE (1k 0.315 ms, 10k
//! 1.660, 100k 3.518). Adding `OFFSET 0` — the same query — answered in
//! 0.159 ms, because OFFSET is one of the shape gates that stands this
//! walk down and sends the statement to the path that seeks.
//!
//! The witness here is a COUNTER, not a clock. `pg_stat_user_tables`
//! separates `idx_scan` from `seq_scan`, and the two walks report
//! different ones — so these tests fail if the seek stops happening,
//! which a wall-clock assertion on a small fixture would not reliably do
//! (round 824). The rest pin what the seek must not change: it only
//! NARROWS, every candidate still goes through the full WHERE, and the
//! answer and its order match the scan's exactly.

use spg_engine::{CancelToken, Engine, StreamItem};

fn run(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

/// Rows as `a|b|c` lines, in the order the walk emitted them.
///
/// STREAMING, deliberately: `Engine::execute` answers through the
/// materialising path, which has always had an index step, so a pin
/// written against it would pass whether or not this change exists. The
/// first version of this file did exactly that — the answers were right
/// and both counter tests failed, because the walk under test had not
/// run at all.
fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
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
    .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    out
}

/// `(idx_scan, seq_scan)` for `t`, as the catalog reports them. Read
/// through the ordinary entry — this is a catalog view, not the walk
/// under test, and reading it through the streaming one would add scans
/// of its own to the numbers being compared.
fn scan_counts(e: &mut Engine) -> (i64, i64) {
    let sql = "SELECT idx_scan, seq_scan FROM pg_stat_user_tables WHERE relname = 't'";
    let r = match e.execute(sql) {
        Ok(spg_engine::QueryResult::Rows { rows, .. }) => rows,
        other => panic!("{sql}: {other:?}"),
    };
    assert_eq!(r.len(), 1, "{sql} -> {r:?}");
    let cell = |i: usize| -> i64 {
        let v = format!("{:?}", r[0].values[i]);
        v.trim_start_matches(|c: char| !c.is_ascii_digit() && c != '-')
            .trim_end_matches(|c: char| !c.is_ascii_digit())
            .parse::<i64>()
            .unwrap_or_else(|_| panic!("not a count: {v}"))
    };
    (cell(0), cell(1))
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE t (id INT PRIMARY KEY, k INT, s TEXT)");
    run(
        &mut e,
        "INSERT INTO t SELECT g, (g*7)%100, CASE WHEN g%13 = 0 THEN NULL ELSE 'v'||g END \
         FROM generate_series(1,300) g",
    );
    run(&mut e, "CREATE INDEX t_k ON t (k)");
    // Dead versions, so the walk has to be right about visibility and not
    // just about arithmetic.
    run(&mut e, "DELETE FROM t WHERE id % 37 = 0");
    run(&mut e, "UPDATE t SET k = k + 500 WHERE id % 11 = 0");
    e
}

#[test]
fn a_point_lookup_reports_an_index_scan_and_no_sequential_scan() {
    let mut e = seeded();
    let (i0, s0) = scan_counts(&mut e);
    let got = rows(&mut e, "SELECT id FROM t WHERE id = 250");
    assert_eq!(got, vec!["Int(250)".to_string()], "{got:?}");
    let (i1, s1) = scan_counts(&mut e);
    assert!(
        i1 > i0,
        "a PK equality must reach an index: idx_scan {i0} -> {i1}"
    );
    assert_eq!(s1, s0, "and must NOT walk the table: seq_scan {s0} -> {s1}");
}

#[test]
fn a_secondary_index_equality_seeks_too() {
    let mut e = seeded();
    let (i0, s0) = scan_counts(&mut e);
    let got = rows(&mut e, "SELECT id FROM t WHERE k = 7");
    let (i1, s1) = scan_counts(&mut e);
    assert!(!got.is_empty(), "the fixture must have matches: {got:?}");
    assert!(i1 > i0, "idx_scan {i0} -> {i1}");
    assert_eq!(s1, s0, "seq_scan {s0} -> {s1}");
}

#[test]
fn a_predicate_no_index_can_serve_still_walks_the_table() {
    let mut e = seeded();
    let (i0, s0) = scan_counts(&mut e);
    let _ = rows(&mut e, "SELECT id FROM t WHERE s LIKE '%9%'");
    let (i1, s1) = scan_counts(&mut e);
    assert_eq!(i1, i0, "nothing to seek on: idx_scan {i0} -> {i1}");
    assert!(s1 > s0, "so it scans: seq_scan {s0} -> {s1}");
}

#[test]
fn the_seek_only_narrows_the_full_where_still_runs() {
    let mut e = seeded();
    // `id = 250` hits the index; `s IS NULL` does not, and 250 is not a
    // multiple of 13, so the row must be filtered OUT. A seek that
    // returned its candidates without re-applying the WHERE answers one
    // row here.
    let got = rows(&mut e, "SELECT id FROM t WHERE id = 250 AND s IS NULL");
    assert!(
        got.is_empty(),
        "the non-indexed conjunct must still hold: {got:?}"
    );

    // And the pair that does hold.
    let got = rows(&mut e, "SELECT id FROM t WHERE id = 260 AND s IS NULL");
    assert_eq!(got, vec!["Int(260)".to_string()], "{got:?}");
}

#[test]
fn a_deleted_row_is_not_resurrected_by_the_index() {
    let mut e = seeded();
    // 37 was deleted. Its index entry may still be there; visibility
    // decides, and it is the same predicate the scan applies.
    let got = rows(&mut e, "SELECT id FROM t WHERE id = 37");
    assert!(got.is_empty(), "deleted: {got:?}");
    // 11 was updated — one live version, not two.
    let got = rows(&mut e, "SELECT id, k FROM t WHERE id = 11");
    assert_eq!(got.len(), 1, "one live version only: {got:?}");
}

#[test]
fn the_seek_answers_exactly_what_the_other_path_answers() {
    let mut e = seeded();
    // `OFFSET 0` changes nothing about the answer and stands this walk
    // down, so the same statement is answered by the materialising path
    // — the pair that named the defect in the first place.
    //
    // Compared as SETS, because the two paths order a multi-row answer
    // differently and always have: this walk emits in table-position
    // order, the materialising one sorts its seek candidates by key, and
    // an UPDATE moves a row's position to the end. Measured over the
    // wire on this exact fixture, round 970: the pre-change and
    // post-change binaries produce byte-identical output for BOTH
    // variants, so the difference is between the two paths and is older
    // than this change. Neither order is promised without an ORDER BY.
    // Asserting the sets is therefore what this test can honestly claim;
    // pinning one path's order to the other's would pin a difference
    // this change did not make and cannot fix here.
    let mut sorted = |sql: &str| -> Vec<String> {
        let mut v = rows(&mut e, sql);
        v.sort();
        v
    };
    for pred in [
        "id = 250",
        "id = 37",
        "k = 7",
        "k = 507",
        "id BETWEEN 100 AND 140",
        "k BETWEEN 10 AND 20",
        "id = 250 AND k > 0",
        "s IS NULL",
    ] {
        let seek = sorted(&format!("SELECT id, k, s FROM t WHERE {pred}"));
        let other = sorted(&format!("SELECT id, k, s FROM t WHERE {pred} OFFSET 0"));
        assert_eq!(seek, other, "WHERE {pred}: the two paths must answer alike");
    }
}

#[test]
fn the_seek_emits_in_table_position_order() {
    let mut e = seeded();
    // What this walk's order IS, pinned directly rather than against the
    // other path. An UPDATE gives a row a new position at the end, so
    // the updated ids trail — which is what the sequential scan this
    // seek replaced also produced, and is why the seek sorts its
    // candidate POSITIONS rather than emitting them in key order.
    let got = rows(&mut e, "SELECT id FROM t WHERE id BETWEEN 100 AND 140");
    let ids: Vec<i64> = got
        .iter()
        .map(|s| {
            s.trim_start_matches("Int(")
                .trim_end_matches(')')
                .parse()
                .expect("an id")
        })
        .collect();
    let updated: Vec<i64> = ids.iter().copied().filter(|i| i % 11 == 0).collect();
    assert_eq!(updated, vec![110, 121, 132], "the fixture's updated ids");
    let tail = &ids[ids.len() - updated.len()..];
    assert_eq!(
        tail, updated,
        "rows an UPDATE moved sit at the end, as the scan left them: {ids:?}"
    );
}
