//! v7.39 (round 564) — the index-only range emits, instead of building a
//! `Vec<Row>` for the encoder to walk once and drop.
//!
//! Round 563 established where this path had to be measured — 10k to
//! 100k rows out, not the 400k where the client bounds both engines and
//! three rounds of work had found 3% of a number that could not move.
//! Profiling there, by INCLUSIVE time rather than self time:
//!
//!     exec_select_cancel_as (the materialising path)   32.9%
//!     try_index_only_range                             ~27%
//!     building the Vec<Row>  (from_iter)               10.2%
//!     dropping the Vec<Row>  (drop_glue)                9.7%
//!     row encoding                                     ~6%
//!
//! A fifth of the connection thread's CPU went on allocating and freeing
//! one single-element `Vec` per output row, so that the wire encoder
//! could borrow each value back out of it for a few nanoseconds. The
//! streaming interface it hands them to takes `&[Value]` already.
//!
//! Measured over pgwire at 50k rows out, three paired batches on one
//! data directory, five runs each:
//!
//!     before  14.00 / 13.54 / 14.80 ms      after  9.77 / 8.86 / 9.76
//!
//! -32%, ranges disjoint (11.73-15.06 against 6.69-10.16). Against PG18
//! across the range, ratio before -> after:
//!
//!     1 row    0.37x -> 0.39x  WIN        50k    2.13x -> 1.48x
//!     1000     0.86x -> 0.66x  WIN       100k    1.81x -> 1.69x
//!     10k      1.95x -> 1.36x            200k    1.58x -> 1.27x
//!
//! This test lives on the SERVER side because that is the only side the
//! streaming path has. The engine's own pins
//! (`e2e_index_only_scan_round560`, `e2e_index_only_runs_round562`)
//! exercise the materialising twin; both go through one shape test and
//! one walk, which is the point of the round's refactor — the rules for
//! what may take this path exist once.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(20);

struct PgMessage {
    ty: u8,
    body: Vec<u8>,
}

fn read_message(s: &mut TcpStream) -> PgMessage {
    let mut ty = [0u8; 1];
    s.read_exact(&mut ty).expect("pg type byte");
    let mut len = [0u8; 4];
    s.read_exact(&mut len).expect("pg length");
    let body_len = u32::from_be_bytes(len).saturating_sub(4) as usize;
    let mut body = vec![0u8; body_len];
    if body_len > 0 {
        s.read_exact(&mut body).expect("pg body");
    }
    PgMessage { ty: ty[0], body }
}

fn connect(addr: &str) -> TcpStream {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let mut body = Vec::new();
    body.extend_from_slice(&196608u32.to_be_bytes());
    body.extend_from_slice(b"user\0postgres\0\0");
    let total = (body.len() + 4) as u32;
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    loop {
        if read_message(&mut s).ty == b'Z' {
            return s;
        }
    }
}

/// Run one simple query; return (column names, every row's first field).
fn rows(s: &mut TcpStream, sql: &str) -> (Vec<String>, Vec<String>) {
    let mut q = vec![b'Q'];
    q.extend_from_slice(&((sql.len() + 5) as u32).to_be_bytes());
    q.extend_from_slice(sql.as_bytes());
    q.push(0);
    s.write_all(&q).unwrap();
    let mut cols = Vec::new();
    let mut out = Vec::new();
    let mut err = None;
    loop {
        let m = read_message(s);
        match m.ty {
            // RowDescription: [u16 n]{ name\0 … 18 bytes }
            b'T' => {
                let n = u16::from_be_bytes([m.body[0], m.body[1]]) as usize;
                let mut p = 2;
                for _ in 0..n {
                    let end = m.body[p..].iter().position(|&b| b == 0).unwrap() + p;
                    cols.push(String::from_utf8_lossy(&m.body[p..end]).into_owned());
                    p = end + 1 + 18;
                }
            }
            // DataRow: [u16 fields][i32 len][bytes]…
            b'D' => {
                let len = i32::from_be_bytes(m.body[2..6].try_into().unwrap());
                out.push(if len < 0 {
                    String::new()
                } else {
                    String::from_utf8_lossy(&m.body[6..6 + len as usize]).into_owned()
                });
            }
            b'E' => err = Some(String::from_utf8_lossy(&m.body).into_owned()),
            b'Z' => {
                assert!(err.is_none(), "{sql}: {err:?}");
                return (cols, out);
            }
            _ => {}
        }
    }
}

#[test]
fn round564_streamed_range_matches_the_materialised_one() {
    let (raw, addrs) = common::ServerBuilder::new().with_pgwire().spawn();
    let mut child = common::ChildGuard(raw);
    let pg = addrs.pgwire.clone().expect("pgwire address");
    let mut c = connect(&pg);

    rows(&mut c, "CREATE TABLE s564 (id INT, k INT, pad TEXT)");
    rows(
        &mut c,
        "INSERT INTO s564 SELECT g, g, 'x' FROM generate_series(1, 4000) g",
    );
    rows(&mut c, "CREATE INDEX s564k ON s564 (k)");
    // Scattered holes, so visibility has to be answered per row and the
    // walk crosses header-trie leaves (32 to a leaf) with gaps in them.
    rows(&mut c, "DELETE FROM s564 WHERE k % 7 = 0");

    let (cols, got) = rows(&mut c, "SELECT k FROM s564 WHERE k BETWEEN 1 AND 3000");
    let want: Vec<String> = (1..=3000)
        .filter(|k| k % 7 != 0)
        .map(|k: i32| k.to_string())
        .collect();
    assert_eq!(cols, vec!["k".to_string()], "one column, named for it");
    assert_eq!(got.len(), want.len(), "row count");
    assert_eq!(got, want, "values, in index order");

    // The alias travels, and the ordinary path agrees on the same range.
    let (cols, aliased) = rows(&mut c, "SELECT k AS n FROM s564 WHERE k BETWEEN 1 AND 3000");
    assert_eq!(cols, vec!["n".to_string()]);
    assert_eq!(aliased, want);
    let (_, via_row) = rows(&mut c, "SELECT id FROM s564 WHERE k BETWEEN 1 AND 3000");
    assert_eq!(via_row, want, "the row-fetch path must not disagree");

    // A short range with a hole in it.
    let (_, short) = rows(&mut c, "SELECT k FROM s564 WHERE k BETWEEN 3001 AND 3006");
    let want_short: Vec<String> = (3001..=3006)
        .filter(|k| k % 7 != 0)
        .map(|k: i32| k.to_string())
        .collect();
    assert_eq!(short, want_short);
    // An empty range still emits a RowDescription — not nothing.
    let (cols, none) = rows(&mut c, "SELECT k FROM s564 WHERE k BETWEEN 90000 AND 90010");
    assert_eq!(cols, vec!["k".to_string()], "header even with no rows");
    assert!(none.is_empty());

    let _ = child.0.kill();
}

/// A type whose key does not restore it keeps the ordinary path — the
/// decision is made before anything is emitted, not discovered halfway.
///
/// Writing this found something much worse than a missing fast path,
/// and it had nothing to do with this round's work: an index on a DATE
/// column made queries against it return NOTHING.
///
///     no index     d = '2026-01-02'    1 row    (PG: 1)
///     with index   d = '2026-01-02'    0 rows   (PG: 1)
///
/// `resolve_col_literal_pair` turned every string literal into
/// `Value::text`, so the seek built an `IndexKey::Text` while the rows
/// under that index are keyed `IndexKey::Int` — days since the epoch. It
/// searched a key space nothing lives in. Two-sided ranges went the same
/// way; a one-sided `d > '…'` survived only because it is never seeked.
/// Date, timestamp, time, uuid and bool columns were all affected, and
/// CREATE INDEX is the one statement that may never change an answer.
#[test]
fn round564_unrestorable_types_fall_back_intact() {
    let (raw, addrs) = common::ServerBuilder::new().with_pgwire().spawn();
    let mut child = common::ChildGuard(raw);
    let pg = addrs.pgwire.clone().expect("pgwire address");
    let mut c = connect(&pg);

    // A date and a timestamp both key as an integer, so neither may be
    // served from the key.
    rows(&mut c, "CREATE TABLE d564 (d DATE, t TEXT)");
    rows(
        &mut c,
        "INSERT INTO d564 VALUES ('2026-01-01', 'a'), ('2026-01-02', 'b'), ('2026-01-03', 'c')",
    );
    rows(&mut c, "CREATE INDEX d564d ON d564 (d)");
    let (cols, got) = rows(
        &mut c,
        "SELECT d FROM d564 WHERE d BETWEEN '2026-01-01' AND '2026-01-02'",
    );
    assert_eq!(cols, vec!["d".to_string()]);
    assert_eq!(
        got,
        vec!["2026-01-01", "2026-01-02"],
        "still dates, not integers"
    );

    // Text does restore, and must come back as text.
    rows(&mut c, "CREATE INDEX d564t ON d564 (t)");
    let (_, t) = rows(&mut c, "SELECT t FROM d564 WHERE t BETWEEN 'a' AND 'b'");
    assert_eq!(t, vec!["a", "b"]);

    let _ = child.0.kill();
}

/// The shapes that must NOT take the streaming path still answer, and
/// answer the same.
#[test]
fn round564_other_shapes_still_answer() {
    let (raw, addrs) = common::ServerBuilder::new().with_pgwire().spawn();
    let mut child = common::ChildGuard(raw);
    let pg = addrs.pgwire.clone().expect("pgwire address");
    let mut c = connect(&pg);

    rows(&mut c, "CREATE TABLE o564 (id INT, k INT)");
    rows(
        &mut c,
        "INSERT INTO o564 SELECT g, g % 50 FROM generate_series(1, 500) g",
    );
    rows(&mut c, "CREATE INDEX o564k ON o564 (k)");

    let (_, ordered) = rows(
        &mut c,
        "SELECT k FROM o564 WHERE k BETWEEN 1 AND 3 ORDER BY k DESC LIMIT 4",
    );
    assert_eq!(ordered, vec!["3", "3", "3", "3"]);
    let (_, distinct) = rows(
        &mut c,
        "SELECT DISTINCT k FROM o564 WHERE k BETWEEN 1 AND 3",
    );
    assert_eq!(distinct.len(), 3);
    let (_, two) = rows(&mut c, "SELECT count(*) FROM o564 WHERE k BETWEEN 1 AND 3");
    assert_eq!(two, vec!["30"]);
    let (_, expr) = rows(&mut c, "SELECT k + 1 FROM o564 WHERE k = 7");
    assert_eq!(expr.len(), 10);
    assert!(expr.iter().all(|v| v == "8"));

    let _ = child.0.kill();
}

/// CREATE INDEX may not change an answer. Every affected type, asked
/// before and after the index exists; every expectation is a PG18
/// reading.
#[test]
fn round564_an_index_does_not_change_the_answer() {
    let (raw, addrs) = common::ServerBuilder::new().with_pgwire().spawn();
    let mut child = common::ChildGuard(raw);
    let pg = addrs.pgwire.clone().expect("pgwire address");
    let mut c = connect(&pg);

    rows(
        &mut c,
        "CREATE TABLE t564 (d DATE, ts TIMESTAMP, u UUID, b BOOL, t TEXT, n INT)",
    );
    rows(
        &mut c,
        "INSERT INTO t564 VALUES          ('2026-01-01','2026-01-01 10:00','11111111-1111-1111-1111-111111111111',true,'a',1),          ('2026-01-02','2026-01-02 10:00','22222222-2222-2222-2222-222222222222',false,'b',2),          ('2026-01-03','2026-01-03 10:00','33333333-3333-3333-3333-333333333333',true,'c',3)",
    );

    let probes: [(&str, &str); 7] = [
        ("SELECT count(*) FROM t564 WHERE d = '2026-01-02'", "1"),
        (
            "SELECT count(*) FROM t564 WHERE d BETWEEN '2026-01-01' AND '2026-01-02'",
            "2",
        ),
        (
            "SELECT count(*) FROM t564 WHERE ts BETWEEN '2026-01-01 00:00' AND '2026-01-02 23:00'",
            "2",
        ),
        (
            "SELECT count(*) FROM t564 WHERE u = '22222222-2222-2222-2222-222222222222'",
            "1",
        ),
        ("SELECT count(*) FROM t564 WHERE b = true", "2"),
        ("SELECT count(*) FROM t564 WHERE n = 2", "1"),
        ("SELECT count(*) FROM t564 WHERE t = 'b'", "1"),
    ];

    for (sql, want) in probes {
        let (_, got) = rows(&mut c, sql);
        assert_eq!(got, vec![want.to_string()], "before any index: {sql}");
    }
    for ddl in [
        "CREATE INDEX t564d ON t564 (d)",
        "CREATE INDEX t564ts ON t564 (ts)",
        "CREATE INDEX t564u ON t564 (u)",
        "CREATE INDEX t564b ON t564 (b)",
        "CREATE INDEX t564n ON t564 (n)",
        "CREATE INDEX t564t ON t564 (t)",
    ] {
        rows(&mut c, ddl);
    }
    for (sql, want) in probes {
        let (_, got) = rows(&mut c, sql);
        assert_eq!(got, vec![want.to_string()], "with the index: {sql}");
    }

    // A literal the column type cannot parse still raises rather than
    // quietly matching nothing — PG raises here too.
    let mut q = vec![b'Q'];
    let sql = "SELECT count(*) FROM t564 WHERE d = '2026-06-31'";
    q.extend_from_slice(&((sql.len() + 5) as u32).to_be_bytes());
    q.extend_from_slice(sql.as_bytes());
    q.push(0);
    c.write_all(&q).unwrap();
    let mut saw_error = false;
    loop {
        let m = read_message(&mut c);
        if m.ty == b'E' {
            saw_error = true;
        }
        if m.ty == b'Z' {
            break;
        }
    }
    assert!(saw_error, "an unparsable date literal must raise");

    let _ = child.0.kill();
}
