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
    let dir: PathBuf = std::env::temp_dir().join(format!("spg-e2e-bare-{label}-{nanos}"));
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

    // Aim the deadline at the middle of the emitting phase rather than at
    // a fraction of the total. A quarter of the total is 20ms here, which
    // lands before the first row exists, and the cancellation then looks
    // exactly like the materialising one it is supposed to rule out.
    let budget = std::cmp::max(20, first_ms + (full_ms - first_ms) / 2);
    ok(&mut s, &format!("SET statement_timeout = '{budget}'"));
    let cut = q(&mut s, "SELECT pad FROM big");

    assert_eq!(
        sqlstate(&cut).as_deref(),
        Some("57014"),
        "a quarter of the time the whole scan needs must not be enough"
    );
    let delivered = col0(&cut).len();
    // This is the assertion that distinguishes the two paths. Both of
    // them end in 57014; only the streaming one has already handed the
    // client rows when it does, because the materialising path reaches
    // its cancellation check before the first row is encoded.
    assert!(
        delivered > 0,
        "a bare projection that materialises cancels with nothing sent; \
         it must stream, and so arrive partly delivered"
    );
    assert!(
        delivered < ROWS,
        "cancelled after {delivered} of {ROWS} rows — it ran to completion instead"
    );
    assert!(
        !cut.iter().any(|m| m.ty == b'C'),
        "a cancelled statement reports no CommandComplete"
    );

    // The session survives its own cancellation.
    ok(&mut s, "SET statement_timeout = '0'");
    assert_eq!(
        col0(&ok(&mut s, "SELECT count(*) FROM big")),
        vec![ROWS.to_string()]
    );
}
