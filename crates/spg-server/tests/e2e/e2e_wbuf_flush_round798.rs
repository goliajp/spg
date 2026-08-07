//! r798 — a large result is written to the socket as it is encoded,
//! not held whole.
//!
//! Measured on a 300k-row table before the change: `SELECT pad FROM big`
//! cost 145 MB of RSS, of which about 63 MB was the write buffer holding
//! every encoded byte until the last row was done. The decomposition
//! that predicted it (63 MB of wire, 72 MB of primary-table copy, 13 MB
//! of per-row work) was written down first and then held: with a
//! megabyte watermark the same query costs 85 MB, and a narrow
//! projection over the same rows costs the same 85 MB, since what is
//! left no longer depends on how wide the rows are.
//!
//! What these pin is the part a measurement cannot: that the rows still
//! all arrive, in order, with their content intact, across every path
//! that encodes rows (streaming, SCALARSQ, materialised), and that the
//! one visible behaviour change is the one intended.
//!
//! That change: once bytes have gone to the socket they cannot be
//! withdrawn, so a query that fails after a flush hands the client the
//! rows it already sent and then an ErrorResponse. PG does exactly that
//! at the protocol level; libpq-based clients discard the rows when the
//! error arrives, so what an application sees is unchanged.
//!
//! The intended behaviour change has no reachable trigger today, and
//! finding that out took four attempts at constructing one. Each failed
//! for its own reason, and together they say something about the paths:
//!
//!   * `ORDER BY` sorts the whole result before encoding anything, so
//!     the error arrived with the buffer still empty;
//!   * putting the failing expression in the projection took the query
//!     off the streaming path altogether — an arithmetic projection
//!     materialises, and failed there, before a row reached the wire;
//!   * the byte ceiling does not trip mid-emit either. It is charged
//!     when the primary table is copied at setup (join.rs, where the
//!     peer's rows are materialised), so a result too large for the
//!     ceiling is refused before the first row is encoded — twice, at
//!     two different ceilings, with no rows on the wire.
//!
//! So for a streaming SELECT, failures happen at setup, and once rows
//! start flowing nothing between here and the last row can fail —
//! except a cancellation, which does check per row and which PG answers
//! with 57014 after the rows it already sent. Pinning that needs a
//! second connection issuing a CancelRequest against the backend key
//! from startup; it is the one case where "rows already on the wire"
//! becomes observable, and it is worth building when something depends
//! on it. What is NOT worth doing is leaving a test here that asserts a
//! scenario the server cannot currently produce: it would pass by
//! accident and say nothing.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(30);

struct PgMessage {
    ty: u8,
    body: Vec<u8>,
}

fn read_message(s: &mut TcpStream) -> PgMessage {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len.saturating_sub(4)];
    if !body.is_empty() {
        s.read_exact(&mut body).expect("body");
    }
    PgMessage { ty, body }
}

fn send_startup(s: &mut TcpStream) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0anyone\0\0");
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn read_until_ready(s: &mut TcpStream) -> Vec<PgMessage> {
    let mut out = Vec::new();
    loop {
        let m = read_message(s);
        let z = m.ty == b'Z';
        out.push(m);
        if z {
            return out;
        }
    }
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s);
    let _ = read_until_ready(&mut s);
    s
}

fn q(s: &mut TcpStream, sql: &str) -> Vec<PgMessage> {
    send_query(s, sql);
    read_until_ready(s)
}

fn col0(msgs: &[PgMessage]) -> Vec<String> {
    let mut out = Vec::new();
    for m in msgs.iter().filter(|m| m.ty == b'D') {
        let len = i32::from_be_bytes([m.body[2], m.body[3], m.body[4], m.body[5]]);
        if len < 0 {
            out.push(String::new());
            continue;
        }
        out.push(String::from_utf8_lossy(&m.body[6..6 + len as usize]).into_owned());
    }
    out
}

fn message_of(msgs: &[PgMessage]) -> String {
    msgs.iter()
        .find(|m| m.ty == b'E')
        .map(|m| String::from_utf8_lossy(&m.body).into_owned())
        .unwrap_or_default()
}

fn sqlstate(msgs: &[PgMessage]) -> Option<String> {
    let m = msgs.iter().find(|m| m.ty == b'E')?;
    let mut i = 0;
    while i < m.body.len() && m.body[i] != 0 {
        let code = m.body[i];
        let start = i + 1;
        let end = start + m.body[start..].iter().position(|&b| b == 0)?;
        if code == b'C' {
            return Some(String::from_utf8_lossy(&m.body[start..end]).into_owned());
        }
        i = end + 1;
    }
    None
}

fn spawn(label: &str) -> (common::ChildGuard, common::ServerAddrs) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir: PathBuf = std::env::temp_dir().join(format!("spg-e2e-flush-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    (common::ChildGuard(raw), addrs)
}

/// 40k rows of 200 bytes is ~8 MB encoded — eight watermarks, so the
/// buffer is flushed and refilled several times mid-result.
fn seed(s: &mut TcpStream) {
    q(s, "CREATE TABLE big (id INT PRIMARY KEY, pad TEXT)");
    q(
        s,
        "INSERT INTO big SELECT g, repeat('y', 200) FROM generate_series(1,40000) g",
    );
}

#[test]
fn every_row_survives_a_result_that_spans_many_flushes() {
    let (_child, addrs) = spawn("streaming");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    seed(&mut s);

    let msgs = q(&mut s, "SELECT id FROM big ORDER BY id");
    assert_eq!(sqlstate(&msgs), None);
    let ids = col0(&msgs);
    assert_eq!(ids.len(), 40000, "no row is lost across the flushes");
    assert_eq!(ids.first().map(String::as_str), Some("1"));
    assert_eq!(ids.last().map(String::as_str), Some("40000"));
    // A boundary-crossing row is intact, not torn: every id parses and
    // the sequence has no gaps.
    for (expected, got) in (1i64..).zip(ids.iter()) {
        assert_eq!(
            got.parse::<i64>().unwrap(),
            expected,
            "ids arrive in order with no gap"
        );
    }
}

#[test]
fn wide_payloads_cross_the_watermark_intact() {
    let (_child, addrs) = spawn("wide");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    seed(&mut s);

    let msgs = q(&mut s, "SELECT pad FROM big");
    assert_eq!(sqlstate(&msgs), None);
    let pads = col0(&msgs);
    assert_eq!(pads.len(), 40000);
    assert!(
        pads.iter().all(|p| p.len() == 200),
        "every payload is its full 200 bytes; a row split by a flush \
         would arrive short"
    );
}

/// The SCALARSQ path (a scalar subquery in the projection) and the
/// materialised path (aggregates, subqueries) encode rows in their own
/// loops. All three flush, or the bound is only true for some queries.
#[test]
fn the_other_encode_paths_flush_too() {
    let (_child, addrs) = spawn("paths");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    seed(&mut s);
    q(&mut s, "CREATE TABLE probe (k INT PRIMARY KEY, n INT)");
    q(&mut s, "INSERT INTO probe VALUES (1, 7)");

    let scalarsq = q(
        &mut s,
        "SELECT (SELECT p.n FROM probe p WHERE p.k = 1) AS a, big.pad FROM big",
    );
    assert_eq!(sqlstate(&scalarsq), None);
    assert_eq!(
        col0(&scalarsq).len(),
        40000,
        "the SCALARSQ path returns every row"
    );

    let materialised = q(&mut s, "SELECT pad FROM big WHERE id IN (SELECT id FROM big)");
    assert_eq!(sqlstate(&materialised), None);
    assert_eq!(
        col0(&materialised).len(),
        40000,
        "the materialised path returns every row"
    );
}

/// Handshake capturing BackendKeyData — the (pid, secret) pair a
/// CancelRequest must echo.
fn open_with_key(addr: &str) -> (TcpStream, u32, u32) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s);
    let mut key = None;
    loop {
        let m = read_message(&mut s);
        match m.ty {
            b'K' => {
                let pid = u32::from_be_bytes(m.body[0..4].try_into().unwrap());
                let secret = u32::from_be_bytes(m.body[4..8].try_into().unwrap());
                key = Some((pid, secret));
            }
            b'Z' => break,
            _ => {}
        }
    }
    let (pid, secret) = key.expect("BackendKeyData before ReadyForQuery");
    (s, pid, secret)
}

fn send_cancel(addr: &str, pid: u32, secret: u32) {
    let mut c = TcpStream::connect(addr).unwrap();
    let mut buf = Vec::with_capacity(16);
    buf.extend_from_slice(&16u32.to_be_bytes());
    buf.extend_from_slice(&80877102u32.to_be_bytes());
    buf.extend_from_slice(&pid.to_be_bytes());
    buf.extend_from_slice(&secret.to_be_bytes());
    c.write_all(&buf).unwrap();
    // The server closes this connection once the cancel is processed —
    // reading to EOF is the acknowledgement, so no sleep is needed
    // between cancelling and draining (r824 established this).
    let _ = c.read(&mut [0u8; 1]);
}

/// The one mid-stream failure a streaming SELECT can actually have
/// (round 798 measured that everything else fails at setup), and the
/// wire contract the incremental flush changes: rows already flushed
/// stay on the wire, followed by 57014, and never a CommandComplete.
///
/// The construction is deterministic rather than timed. The client
/// reads a little and then STOPS: TCP backpressure fills the socket
/// buffers (a few hundred KB) and blocks the server mid-flush, far
/// short of the ~8 MB result. The cancel flag is set while the server
/// is stalled; the moment the client drains again, the server unblocks
/// and the next per-row cancel check fails. Rows-then-error is thereby
/// guaranteed, not raced for.
#[test]
fn a_cancel_mid_drain_keeps_the_flushed_rows_and_ends_in_57014() {
    let (_child, addrs) = spawn("cancel");
    let addr = addrs.pgwire.as_ref().unwrap().clone();
    let (mut s, pid, secret) = open_with_key(&addr);
    // Not seed()'s 8 MB: macOS loopback buffering autotunes far enough
    // to swallow that whole, and the first run of this test measured
    // exactly that — rows=40000, complete=true, no backpressure ever.
    // ~80 MB encoded cannot fit anywhere, so the server MUST block.
    // Seeding must be CHECKED: a swallowed INSERT error here once left
    // the table empty, the SELECT answered with two frames, and the
    // fixed 20-frame read below sat on frame 3 for the full timeout —
    // a 30 s stall that pointed at everything except the actual error.
    let c = q(&mut s, "CREATE TABLE big (id INT PRIMARY KEY, pad TEXT)");
    assert_eq!(sqlstate(&c), None, "create failed: {}", message_of(&c));
    for start in [1u32, 100_001, 200_001, 300_001] {
        let ins = q(
            &mut s,
            &format!(
                "INSERT INTO big SELECT g, repeat('y', 200) FROM \
                 generate_series({start},{}) g",
                start + 99_999
            ),
        );
        assert_eq!(
            sqlstate(&ins),
            None,
            "insert from {start} failed: {}",
            message_of(&ins)
        );
    }

    send_query(&mut s, "SELECT pad FROM big");
    // Read a few frames so rows are provably on the wire, then stall.
    // Bounded by what actually arrives — never a fixed count, which is
    // exactly how the seeding failure above turned into a silent hang.
    let mut early_rows = 0usize;
    while early_rows < 15 {
        let m = read_message(&mut s);
        match m.ty {
            b'D' => early_rows += 1,
            b'E' => panic!(
                "the SELECT errored before any stall: {}",
                String::from_utf8_lossy(&m.body)
            ),
            b'Z' => panic!("the SELECT finished in under 15 rows"),
            _ => {}
        }
    }

    send_cancel(&addr, pid, secret);

    // Drain: more rows (whatever was already encoded), then the error.
    let mut rows = early_rows;
    let mut error: Option<PgMessage> = None;
    let mut complete = false;
    loop {
        let m = read_message(&mut s);
        match m.ty {
            b'D' => rows += 1,
            b'E' => error = Some(m),
            b'C' => complete = true,
            b'Z' => break,
            _ => {}
        }
    }
    let e = error.unwrap_or_else(|| {
        panic!(
            "no ErrorResponse: the query ignored the cancel and finished \
             (rows={rows}, complete={complete}) — either the flag never \
             reached this session or the drain outran the flag"
        )
    });
    let text = String::from_utf8_lossy(&e.body).to_string();
    assert!(text.contains("57014"), "PG's query_canceled, got {text:?}");
    assert!(
        rows < 400_000,
        "the query must not have finished; {rows} rows arrived"
    );
    assert!(!complete, "a cancelled SELECT owes no CommandComplete");

    // And the connection is still usable — PG's cancel kills the
    // statement, not the session.
    let after = q(&mut s, "SELECT count(*) FROM big");
    assert_eq!(sqlstate(&after), None);
    assert_eq!(col0(&after), vec!["400000"]);
}

