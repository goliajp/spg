//! r823 — an unqualified column name resolves the same way on both
//! resolution paths, so a bare projection still streams.
//!
//! SPG has two column resolvers. `resolve_column` does the general
//! thing, and `find_column_pos` binds a position once for the fast
//! paths that then read that position per row. In a joined or deferred
//! context the synthesised schema names columns `"alias.column"`, and
//! only the general resolver knew to fall back to matching a single
//! composite column ending in `".<name>"`. The bind-once one did not,
//! so it answered "no such column" for a name that plainly exists.
//!
//! Nothing about that read as a bug at the call sites: the binder is
//! allowed to decline, and declining means "use the general path". So
//! `SELECT pad FROM big` — the commonest projection there is — quietly
//! left the streaming path and materialised instead. Measured with
//! `statement_timeout = 120ms` over 400k rows: the bare form ran to
//! completion, all 400000 rows, 0.81s, no error; `big.pad` and `b.pad`
//! over the same table cancelled at ~65k rows in 0.14s. The timeout was
//! not being ignored — the query was simply on a path that reaches its
//! cancellation check only after building the whole result.
//!
//! So what looked like a cancellation defect was a resolver
//! disagreement, and the fix is to make the two resolvers agree rather
//! than to add a second cancellation check.
//!
//! These pin both halves of that. The values and the SQLSTATE say the
//! agreement is PG's agreement and not merely self-consistent: PG18
//! resolves the same bare names to the same values and answers 42702
//! for the ambiguous ones. The streaming pin says the bare form is back
//! on the path that can be interrupted — it is the only one of the
//! three that a resolver regression would silently undo.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const READ_TIMEOUT: Duration = Duration::from_secs(60);

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

fn q(s: &mut TcpStream, sql: &str) -> Vec<PgMessage> {
    send_query(s, sql);
    read_until_ready(s)
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s);
    // The startup handshake ends in its own ReadyForQuery. Draining it
    // with a query instead would answer that query with the handshake's
    // frames and leave every later statement reading the previous one's.
    let _ = read_until_ready(&mut s);
    s
}

/// Like `open`, also returning the BackendKeyData a CancelRequest needs.
fn open_with_key(addr: &str) -> (TcpStream, (u32, u32)) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s);
    let msgs = read_until_ready(&mut s);
    let k = msgs
        .iter()
        .find(|m| m.ty == b'K')
        .expect("BackendKeyData in the handshake");
    let pid = u32::from_be_bytes([k.body[0], k.body[1], k.body[2], k.body[3]]);
    let secret = u32::from_be_bytes([k.body[4], k.body[5], k.body[6], k.body[7]]);
    (s, (pid, secret))
}

/// A CancelRequest arrives on its own connection, as PG's protocol says.
fn send_cancel(addr: &str, (pid, secret): (u32, u32)) {
    let mut c = TcpStream::connect(addr).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&16_u32.to_be_bytes());
    out.extend_from_slice(&80_877_102_u32.to_be_bytes());
    out.extend_from_slice(&pid.to_be_bytes());
    out.extend_from_slice(&secret.to_be_bytes());
    c.write_all(&out).unwrap();
    let _ = c.read(&mut [0u8; 1]);
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

fn field_names(msgs: &[PgMessage]) -> Vec<String> {
    let Some(m) = msgs.iter().find(|m| m.ty == b'T') else {
        return Vec::new();
    };
    let count = u16::from_be_bytes([m.body[0], m.body[1]]) as usize;
    let mut out = Vec::with_capacity(count);
    let mut i = 2;
    for _ in 0..count {
        let end = i + m.body[i..].iter().position(|&b| b == 0).unwrap();
        out.push(String::from_utf8_lossy(&m.body[i..end]).into_owned());
        // name, then the fixed 18-byte descriptor PG sends per field.
        i = end + 1 + 18;
    }
    out
}

fn error_field(msgs: &[PgMessage], want: u8) -> Option<String> {
    let m = msgs.iter().find(|m| m.ty == b'E')?;
    let mut i = 0;
    while i < m.body.len() && m.body[i] != 0 {
        let code = m.body[i];
        let start = i + 1;
        let end = start + m.body[start..].iter().position(|&b| b == 0)?;
        if code == want {
            return Some(String::from_utf8_lossy(&m.body[start..end]).into_owned());
        }
        i = end + 1;
    }
    None
}

fn sqlstate(msgs: &[PgMessage]) -> Option<String> {
    error_field(msgs, b'C')
}

fn message_of(msgs: &[PgMessage]) -> String {
    error_field(msgs, b'M').unwrap_or_default()
}

fn spawn(label: &str) -> (common::ChildGuard, common::ServerAddrs) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir: PathBuf = crate::common::tmp_base().join(format!("spg-e2e-bare-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    (common::ChildGuard(raw), addrs)
}

fn ok(s: &mut TcpStream, sql: &str) -> Vec<PgMessage> {
    let msgs = q(s, sql);
    assert_eq!(
        sqlstate(&msgs),
        None,
        "setup statement failed: {sql} -> {}",
        message_of(&msgs)
    );
    msgs
}

/// Two relations sharing a column name, so `v` is ambiguous across the
/// join while `w` and `hint` are not.
fn seed_pair(s: &mut TcpStream) {
    ok(s, "CREATE TABLE r823a (id INT, v TEXT, hint TEXT)");
    ok(s, "CREATE TABLE r823b (id INT, w TEXT, v TEXT)");
    ok(s, "INSERT INTO r823a VALUES (1,'a1','h1'),(2,'a2','h2')");
    ok(s, "INSERT INTO r823b VALUES (1,'w1','bv1'),(2,'w2','bv2')");
}

const JOIN: &str = "FROM r823a x JOIN r823b y ON x.id = y.id";

#[test]
fn a_bare_column_in_a_joined_context_reads_the_same_as_its_qualified_form() {
    let (_child, addrs) = spawn("agree");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    seed_pair(&mut s);

    // `w` lives only in y, `hint` only in x: both are unambiguous bare
    // names that the bind-once resolver used to decline.
    for (bare, qualified) in [("w", "y.w"), ("hint", "x.hint")] {
        let bare_rows = ok(&mut s, &format!("SELECT {bare} {JOIN} ORDER BY 1"));
        let qual_rows = ok(&mut s, &format!("SELECT {qualified} {JOIN} ORDER BY 1"));
        assert_eq!(
            col0(&bare_rows),
            col0(&qual_rows),
            "`{bare}` and `{qualified}` must name the same column"
        );
        assert!(!col0(&bare_rows).is_empty(), "`{bare}` returned nothing");

        // The composite name is an internal spelling. What goes out on
        // the wire is the name the client asked for — a client that
        // indexes its result by column name would break silently, not
        // loudly, if this became "y.w".
        assert_eq!(
            field_names(&bare_rows),
            vec![bare.to_string()],
            "the field name stays unqualified, as PG18 sends it"
        );
    }

    // The same bare name works where it is not the projection, which is
    // the case the general resolver already covered — pinned so a change
    // to the shared fallback cannot fix one and break the other.
    let filtered = ok(&mut s, &format!("SELECT y.w {JOIN} WHERE w = 'w2'"));
    assert_eq!(col0(&filtered), vec!["w2".to_string()]);
}

#[test]
fn a_bare_column_that_matches_two_relations_is_ambiguous_not_undefined() {
    let (_child, addrs) = spawn("ambiguous");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    seed_pair(&mut s);

    // Both relations have `v`. The fallback declines rather than picking
    // one, which sends the query down the general path and raises this.
    let msgs = q(&mut s, &format!("SELECT v {JOIN} ORDER BY 1"));
    assert_eq!(
        sqlstate(&msgs).as_deref(),
        Some("42702"),
        "PG18 answers 42702 AMBIGUOUS_COLUMN here; 42883 told the client \
         'no such function' for a name-resolution problem"
    );
    assert_eq!(
        message_of(&msgs),
        "column reference \"v\" is ambiguous",
        "PG's own sentence, so a client matching on text sees no difference"
    );
    assert!(
        col0(&msgs).is_empty(),
        "an ambiguous reference resolves to no rows at all, never to a guess"
    );
}

/// The observable that DOES separate streaming from materialising:
/// what the server's memory does while the answer is being sent.
///
/// Round 856 established that the wire cannot tell them apart — round
/// 824's `emit_materialised` hands rows to the consumer one at a time,
/// so both paths deliver early and both cancel between rows. Memory is
/// where they differ, because one of them holds the whole result while
/// the other holds a row.
///
/// 200k rows of 200 bytes is a 40 MB answer. A path that builds it
/// whole cannot do so in under 20 MB; a path that walks it barely moves.
/// The threshold is half the result, so it is the SHAPE of the query
/// that decides this, not the speed of the machine — the same reason
/// round 857 preferred an order to a ratio.
///
/// Round 859 checked it: with every streaming entry gated off, the
/// server grew 89 MiB for a 38 MiB answer — about 2.3x the result, the
/// shape of whole rows collected into a `Vec<Row>` and then encoded —
/// and this goes red. Under the streaming walk it stays under 20.
#[test]
fn a_bare_projection_walks_the_table_instead_of_copying_it() {
    let (child, addrs) = spawn("rss");
    let pid = child.0.id();
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    ok(&mut s, "CREATE TABLE big (id INT PRIMARY KEY, pad TEXT)");
    const ROWS: usize = 200_000;
    ok(
        &mut s,
        &format!("INSERT INTO big SELECT g, repeat('y', 200) FROM generate_series(1,{ROWS}) g"),
    );

    // Settle first: the insert's own buffers are not what is being
    // measured, and a baseline taken while they drain would flatter the
    // reading.
    for _ in 0..20 {
        if col0(&ok(&mut s, "SELECT count(*) FROM big")) == vec![ROWS.to_string()] {
            break;
        }
    }
    let base_kib = common::rss_kib_of(pid);
    assert!(base_kib > 0, "could not read the server's RSS");

    // Sample while the rows are in flight. The peak is the point: a
    // materialising path is at its widest with the result assembled and
    // nothing sent yet, which a reading taken afterwards would miss
    // entirely.
    let peak = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler = {
        let (peak, stop) = (peak.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let now = common::rss_kib_of(pid);
                peak.fetch_max(now, std::sync::atomic::Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };

    send_query(&mut s, "SELECT pad FROM big");
    let mut rows = 0usize;
    loop {
        let m = read_message(&mut s);
        match m.ty {
            b'D' => rows += 1,
            b'E' => panic!("the scan errored: {}", message_of(&[m])),
            b'Z' => break,
            _ => {}
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    sampler.join().unwrap();
    assert_eq!(rows, ROWS, "the whole table came back");

    let peak_kib = peak.load(std::sync::atomic::Ordering::Relaxed);
    let grew_mib = peak_kib.saturating_sub(base_kib) / 1024;
    assert!(
        grew_mib < 20,
        "the server grew {grew_mib} MiB while sending a {} MiB result: it \
         built the answer whole instead of walking the table",
        (ROWS * 200) / (1024 * 1024)
    );
}

/// ⚠️ What this pins, and what it does NOT.
///
/// The name promises the bare projection STREAMS rather than being built
/// whole. Round 856 put that to a negative control — every streaming
/// entry disabled at once, `try_stream_single_table` and both call sites
/// of `try_exec_joined_streaming` — and the test went on passing, in
/// this form and in the form it had before.
///
/// The reason is that round 824 gave the materialising path
/// `emit_materialised`, which hands rows to the consumer one at a time.
/// Both paths therefore look the same on the wire: rows arrive early,
/// and a cancel lands between two of them. The premise that the wire can
/// tell them apart stopped holding the day that landed, and this test
/// has been passing for the wrong reason since.
///
/// So take it for what it does hold: the projection answers, the first
/// row arrives long before the last, a cancel mid-flight ends in 57014
/// with part of the result delivered and no CommandComplete, and the
/// session survives. All true, all worth keeping, none of it specific to
/// streaming.
///
/// The observable that DOES separate them is peak memory — round 831
/// measured this exact query at +117 MB materialised against +0 MB
/// streamed — and pinning that needs a test that watches the server's
/// RSS, which is a different shape from this file.
#[test]
fn a_bare_projection_streams_so_a_timeout_can_still_interrupt_it() {
    let (_child, addrs) = spawn("streaming");
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    ok(&mut s, "CREATE TABLE big (id INT PRIMARY KEY, pad TEXT)");
    const ROWS: usize = 80_000;
    ok(
        &mut s,
        &format!("INSERT INTO big SELECT g, repeat('y', 200) FROM generate_series(1,{ROWS}) g"),
    );

    // Calibrate against this machine instead of hard-coding a
    // millisecond count: a debug build on a loaded box is slower than
    // the release build this was measured on, and a fixed timeout would
    // then be either always or never reached.
    let started = Instant::now();
    send_query(&mut s, "SELECT pad FROM big");
    let mut first_row = None;
    let mut untimed_rows = 0usize;
    loop {
        let m = read_message(&mut s);
        if m.ty == b'D' {
            if first_row.is_none() {
                first_row = Some(started.elapsed());
            }
            untimed_rows += 1;
        }
        if m.ty == b'Z' {
            break;
        }
    }
    let full_ms = started.elapsed().as_millis();
    let first_ms = first_row.expect("at least one row arrives").as_millis();
    assert_eq!(untimed_rows, ROWS, "the untimed run returns everything");

    // The first row arriving well before the last one IS the streaming
    // property, stated without reference to any clock speed: a result
    // that is built whole before anything is sent cannot do this. On a
    // debug build over these 80k rows the first row landed at 29ms of
    // 82ms — a third of the way — where materialising would put it at
    // the end.
    assert!(
        first_ms * 4 < full_ms * 3,
        "first row after {first_ms}ms of {full_ms}ms: the result was built \
         whole before any of it was sent"
    );

    // Cancel in response to something OBSERVED rather than at a moment
    // guessed from an earlier run's clock.
    //
    // The budget used to be derived from the calibration run above and
    // then applied to this one. When the machine's load moved between
    // the two — a neighbouring build taking twelve cores, in the run
    // that exposed this — the second pass finished inside a budget sized
    // for a slower first pass, and the test reported a defect that was
    // not there. Round 824 met the same thing on its own file and
    // switched to a CancelRequest; this file kept the deadline, which is
    // the whole of why it was still flaky.
    let pg = addrs.pgwire.as_ref().unwrap();
    let (mut c, key) = open_with_key(pg);
    send_query(&mut c, "SELECT pad FROM big");
    let mut delivered = 0usize;
    while delivered < 1_000 {
        let m = read_message(&mut c);
        match m.ty {
            b'D' => delivered += 1,
            b'E' => panic!("the scan errored before rows flowed: {}", message_of(&[m])),
            b'Z' => panic!("the scan finished before {delivered} rows could be read"),
            _ => {}
        }
    }
    send_cancel(pg, key);
    let mut code = None;
    let mut complete = false;
    loop {
        let m = read_message(&mut c);
        match m.ty {
            b'D' => delivered += 1,
            b'C' => complete = true,
            b'E' => code = sqlstate(&[m]),
            b'Z' => break,
            _ => {}
        }
    }

    assert_eq!(
        code.as_deref(),
        Some("57014"),
        "rows were already flowing, so the cancel has to land between two \
         of them and end the statement"
    );
    // What distinguishes the two paths. Both end in 57014; only the
    // streaming one has already handed the client rows when it does,
    // because the materialising path reaches its cancellation check
    // before the first row is encoded — and would have sent the whole
    // result before this cancel could arrive at all.
    assert!(
        delivered < ROWS,
        "cancelled after {delivered} of {ROWS} rows — it ran to completion instead"
    );
    assert!(
        !complete,
        "a cancelled statement reports no CommandComplete"
    );

    // The session survives its own cancellation.
    ok(&mut s, "SET statement_timeout = '0'");
    assert_eq!(
        col0(&ok(&mut s, "SELECT count(*) FROM big")),
        vec![ROWS.to_string()]
    );
}
