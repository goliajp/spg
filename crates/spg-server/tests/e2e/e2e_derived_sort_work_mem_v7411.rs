//! v7.40.11 — a sort inside a derived table did not honour `work_mem`.
//!
//! Reported against 7.40.9 (§3.7), and it began as the reporter's own
//! correction of a wrong reading: they had filed the `quicksort` LABEL
//! as the defect, and the label is honest — the behaviour underneath it
//! is unbounded, which is worse. A subquery sorting a large table uses
//! memory proportional to the table on a server configured not to.
//!
//! The witness is the engine's own spill counter, read through
//! `pg_stat_database.temp_files` — one file per sort run, the same
//! thing PostgreSQL counts there — around **plain statements**.
//!
//! Not `EXPLAIN ANALYZE`: that road spills on its own. Measured while
//! writing this, at `work_mem = 64 kB` over the same 40k rows —
//! `EXPLAIN ANALYZE SELECT … FROM ( … ORDER BY … ) z` moved the
//! counter by 5 while the PLAIN statement moved it by 0. A pin built on
//! the instrumented road would have been green against the defect.
//!
//! Measured on 7.40.10, `work_mem = 64 kB`, temp_files delta:
//!
//! ```text
//!   SELECT t FROM s ORDER BY t                       +5   the budget is read
//!   SELECT count(*) FROM (SELECT t FROM s ORDER BY t) z   +0   it is not
//! ```
//!
//! Two roads: the streaming entry tries the bounded sort and falls back
//! for the shapes it declines, and the derived-table materialiser
//! called the fallback directly. So the budget reached one and not the
//! other.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(120);

fn unique_db() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = common::tmp_base().join(format!("spg-derived-workmem-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p.join("d.spgdb")
}

fn pg_msg(s: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("pg header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len.saturating_sub(4)];
    if !body.is_empty() {
        s.read_exact(&mut body).expect("pg body");
    }
    (ty, body)
}

fn pg_connect(addr: &str) -> TcpStream {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let mut body = Vec::new();
    body.extend_from_slice(&196_608u32.to_be_bytes());
    body.extend_from_slice(b"user\0bench\0\0");
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
    loop {
        if pg_msg(&mut s).0 == b'Z' {
            break;
        }
    }
    s
}

/// Run one statement over the simple query protocol; return the first
/// field of every row.
fn rows(s: &mut TcpStream, sql: &str) -> Vec<String> {
    let mut q: Vec<u8> = vec![b'Q'];
    let mut b = sql.as_bytes().to_vec();
    b.push(0);
    q.extend_from_slice(&((b.len() + 4) as u32).to_be_bytes());
    q.extend_from_slice(&b);
    s.write_all(&q).unwrap();
    let mut out = Vec::new();
    let mut err = None;
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'D' => {
                let len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
                out.push(if len < 0 {
                    String::new()
                } else {
                    String::from_utf8_lossy(&body[6..6 + len as usize]).into_owned()
                });
            }
            b'E' => {
                let mut pos = 0;
                while pos < body.len() && body[pos] != 0 {
                    let t = body[pos];
                    pos += 1;
                    let end = body[pos..].iter().position(|&c| c == 0).unwrap() + pos;
                    if t == b'M' {
                        err = Some(String::from_utf8_lossy(&body[pos..end]).into_owned());
                    }
                    pos = end + 1;
                }
            }
            b'Z' => break,
            _ => {}
        }
    }
    assert!(err.is_none(), "{sql}: {err:?}");
    out
}

/// The engine's own count of sort runs written to disk, as
/// `pg_stat_database` reports it.
fn temp_files(s: &mut TcpStream) -> i64 {
    let got = rows(s, "SELECT temp_files FROM pg_stat_database LIMIT 1");
    got.first()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or_else(|| panic!("temp_files unreadable: {got:?}"))
}

/// How many runs one PLAIN statement wrote.
fn spilled_by(s: &mut TcpStream, sql: &str) -> i64 {
    let before = temp_files(s);
    rows(s, sql);
    temp_files(s) - before
}

fn seeded() -> (std::process::Child, TcpStream) {
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&unique_db())
        .with_pgwire()
        .spawn();
    let mut s = pg_connect(addrs.pgwire.as_ref().unwrap());
    rows(&mut s, "CREATE TABLE s (id int, t text)");
    // ~2.6 MB of text: far past a 64 kB budget, far inside a 64 MB one.
    rows(
        &mut s,
        "INSERT INTO s SELECT g, md5(g::text) || md5((g*7)::text) \
         FROM generate_series(1,40000) g",
    );
    (raw, s)
}

/// The finding, and its control in the same run: at one budget, the
/// same sort must spill whether or not a derived table wraps it.
#[test]
fn a_sort_inside_a_derived_table_honours_work_mem() {
    let (raw, mut s) = seeded();
    let _guard = common::ChildGuard(raw);
    rows(&mut s, "SET work_mem = 64");

    // The control: the budget reaches this road, and always did.
    let top = spilled_by(&mut s, "SELECT t FROM s ORDER BY t");
    assert!(
        top > 0,
        "the control must spill, or the fixture is too small for the budget"
    );

    // The finding: the same sort, one derived table deep.
    let derived = spilled_by(
        &mut s,
        "SELECT count(*) FROM (SELECT t FROM s ORDER BY t) z",
    );
    assert!(
        derived > 0,
        "a sort inside a derived table wrote {derived} runs where the same \
         sort at the top level wrote {top}"
    );
}

/// The negative control: with a budget the sort fits inside, NEITHER
/// road spills. Without it, a fix that simply always spills would pass
/// the test above.
#[test]
fn a_budget_that_fits_spills_on_neither_road() {
    let (raw, mut s) = seeded();
    let _guard = common::ChildGuard(raw);
    rows(&mut s, "SET work_mem = 65536");

    assert_eq!(
        spilled_by(&mut s, "SELECT t FROM s ORDER BY t"),
        0,
        "64 MB holds 2.6 MB of text"
    );
    assert_eq!(
        spilled_by(
            &mut s,
            "SELECT count(*) FROM (SELECT t FROM s ORDER BY t) z"
        ),
        0,
        "and it holds it inside a derived table too"
    );
}

/// The derived table's ANSWER does not change — a bounded sort that
/// reorders or drops rows would be a far worse defect than the one it
/// fixes.
#[test]
fn the_rows_are_the_same_at_either_budget() {
    let (raw, mut s) = seeded();
    let _guard = common::ChildGuard(raw);
    let q = "SELECT t FROM (SELECT t FROM s ORDER BY t) z LIMIT 5";
    rows(&mut s, "SET work_mem = 65536");
    let fits = rows(&mut s, q);
    rows(&mut s, "SET work_mem = 64");
    let spills = rows(&mut s, q);
    assert_eq!(fits, spills, "the budget must not change the answer");
    assert_eq!(fits.len(), 5);
    let mut sorted = fits.clone();
    sorted.sort();
    assert_eq!(fits, sorted, "and the order is still the order asked for");

    // The whole relation, not just its first five: a spill that loses a
    // run would shorten it.
    let n = rows(
        &mut s,
        "SELECT count(*) FROM (SELECT t FROM s ORDER BY t) z",
    );
    assert_eq!(n, vec!["40000".to_string()]);
}

/// The shape the reporter's own repro uses — a derived table on the
/// join side — takes the same road.
#[test]
fn a_derived_table_on_the_join_side_honours_it_too() {
    let (raw, mut s) = seeded();
    let _guard = common::ChildGuard(raw);
    rows(&mut s, "SET work_mem = 64");
    let n = spilled_by(
        &mut s,
        "SELECT count(*) FROM s a JOIN (SELECT t FROM s ORDER BY t) z ON a.t = z.t",
    );
    assert!(n > 0, "a join's derived side sorted unbounded");
}
