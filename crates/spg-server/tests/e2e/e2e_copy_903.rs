//! 9.0.3 — COPY, re-measured on the published 9.0.2 image against
//! PostgreSQL 18.6. Every expected value below is PostgreSQL's answer
//! to the same statement.
//!
//! The head of a COPY was read by hand from lowercased text and its
//! options by searching for the word `with`. That is where most of these
//! came from:
//!
//! ```text
//!   COPY t TO STDOUT (FORMAT csv, HEADER)      9.0.2: text, no header (WITH is optional)
//!   COPY t TO STDOUT CSV HEADER                every build: text, no header
//!   search_path sa,public; COPY public.t …     9.0.0+: read AND WROTE sa.t
//!   COPY "MixedCase" …                         relation "mixedcase" does not exist
//!   an empty line in the data                  skipped; PostgreSQL reads it as a row
//!   '1.5' into an integer column               stored 2; PostgreSQL refuses it
//!   '+5' into a text column                    refused; PostgreSQL stores +5
//!   ON_ERROR ignore                            accepted, then behaved as stop
//!   BEGIN; CREATE TABLE x; COPY x FROM STDIN   relation "x" does not exist —
//!                                              `psql -1` restored no COPY at all
//! ```

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(10);

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

fn send_msg(s: &mut TcpStream, ty: u8, body: &[u8]) {
    let mut out = vec![ty];
    out.extend_from_slice(&(body.len() as u32 + 4).to_be_bytes());
    out.extend_from_slice(body);
    s.write_all(&out).expect("write");
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let mut startup = Vec::new();
    startup.extend_from_slice(&196_608u32.to_be_bytes());
    for (k, v) in [("user", "postgres"), ("database", "probe")] {
        startup.extend_from_slice(k.as_bytes());
        startup.push(0);
        startup.extend_from_slice(v.as_bytes());
        startup.push(0);
    }
    startup.push(0);
    let mut framed = ((startup.len() + 4) as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(&startup);
    s.write_all(&framed).unwrap();
    loop {
        if read_message(&mut s).ty == b'Z' {
            break;
        }
    }
    s
}

/// A field of an ErrorResponse / NoticeResponse.
fn field(body: &[u8], code: u8) -> Option<String> {
    let mut i = 0;
    while i < body.len() && body[i] != 0 {
        let c = body[i];
        let start = i + 1;
        let end = body[start..]
            .iter()
            .position(|&b| b == 0)
            .map_or(body.len(), |p| start + p);
        if c == code {
            return Some(String::from_utf8_lossy(&body[start..end]).into_owned());
        }
        i = end + 1;
    }
    None
}

/// What one COPY answered.
#[derive(Debug, Default)]
struct Copied {
    /// The CopyData PostgreSQL would print, concatenated.
    data: String,
    /// `SQLSTATE: message` of the error, if any.
    error: Option<String>,
    notices: Vec<String>,
    tag: Option<String>,
    /// DataRows, columns joined by `|`, NULL as empty, one per line.
    rows: String,
}

/// Run a simple-protocol statement; a COPY FROM STDIN is fed `input`
/// as one CopyData frame and CopyDone.
fn run(s: &mut TcpStream, sql: &str, input: &str) -> Copied {
    let mut body = sql.as_bytes().to_vec();
    body.push(0);
    send_msg(s, b'Q', &body);
    let mut out = Copied::default();
    let mut readies = 0;
    loop {
        let m = read_message(s);
        match m.ty {
            b'G' => {
                if !input.is_empty() {
                    send_msg(s, b'd', input.as_bytes());
                }
                send_msg(s, b'c', &[]);
            }
            b'd' => out.data.push_str(&String::from_utf8_lossy(&m.body)),
            b'D' => {
                let n = i16::from_be_bytes([m.body[0], m.body[1]]);
                let mut at = 2usize;
                let mut cols = Vec::new();
                for _ in 0..n {
                    let len = i32::from_be_bytes([
                        m.body[at],
                        m.body[at + 1],
                        m.body[at + 2],
                        m.body[at + 3],
                    ]);
                    at += 4;
                    if len < 0 {
                        cols.push(String::new());
                    } else {
                        let len = len as usize;
                        cols.push(String::from_utf8_lossy(&m.body[at..at + len]).into_owned());
                        at += len;
                    }
                }
                out.rows.push_str(&cols.join("|"));
                out.rows.push('\n');
            }
            b'E' => {
                out.error = Some(format!(
                    "{}: {}",
                    field(&m.body, b'C').unwrap_or_default(),
                    field(&m.body, b'M').unwrap_or_default()
                ));
            }
            b'N' => out.notices.push(field(&m.body, b'M').unwrap_or_default()),
            b'C' => {
                out.tag = Some(
                    String::from_utf8_lossy(&m.body)
                        .trim_end_matches('\0')
                        .to_string(),
                );
            }
            b'Z' => {
                readies += 1;
                break;
            }
            _ => {}
        }
    }
    assert_eq!(readies, 1, "{sql}: exactly one ReadyForQuery");
    out
}

fn ok(s: &mut TcpStream, sql: &str) {
    let r = run(s, sql, "");
    assert!(r.error.is_none(), "{sql}: {:?}", r.error);
}

/// The rows of a query, read as DataRows — a path independent of COPY.
fn rows(s: &mut TcpStream, sql: &str) -> String {
    let r = run(s, sql, "");
    assert!(r.error.is_none(), "{sql}: {:?}", r.error);
    r.rows
}

fn server() -> (common::ChildGuard, TcpStream) {
    let (child, c, _) = server_in();
    (child, c)
}

fn server_in() -> (common::ChildGuard, TcpStream, std::path::PathBuf) {
    let dir = crate::common::tmp_base().join(format!(
        "spg-e2e-copy903-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let (raw, addrs) = common::ServerBuilder::new()
        .arg_path(&dir.join("spg.db"))
        .with_pgwire()
        .spawn();
    let child = common::ChildGuard(raw);
    let c = open(addrs.pgwire.as_ref().unwrap());
    (child, c, dir)
}

#[test]
fn the_options_are_read_with_or_without_with_and_in_the_legacy_spelling() {
    let (_child, mut c) = server();
    ok(&mut c, "CREATE TABLE t (a int, b text)");
    ok(&mut c, "INSERT INTO t VALUES (1, 'x'), (2, 'a\"b')");
    for (sql, want) in [
        (
            "COPY t TO STDOUT (FORMAT csv, HEADER)",
            "a,b\n1,x\n2,\"a\"\"b\"\n",
        ),
        (
            "COPY t TO STDOUT WITH (FORMAT csv, HEADER)",
            "a,b\n1,x\n2,\"a\"\"b\"\n",
        ),
        ("COPY t TO STDOUT CSV HEADER", "a,b\n1,x\n2,\"a\"\"b\"\n"),
        (
            "COPY t TO STDOUT DELIMITER AS '|' HEADER",
            "a|b\n1|x\n2|a\"b\n",
        ),
        (
            "COPY t TO STDOUT CSV FORCE QUOTE b",
            "1,\"x\"\n2,\"a\"\"b\"\n",
        ),
        (
            "COPY t TO STDOUT (FORMAT csv, ESCAPE '\\', FORCE_QUOTE *)",
            "\"1\",\"x\"\n\"2\",\"a\\\"b\"\n",
        ),
        ("COPY t (b) TO STDOUT WITH CSV", "x\n\"a\"\"b\"\n"),
    ] {
        let r = run(&mut c, sql, "");
        assert_eq!(r.error, None, "{sql}");
        assert_eq!(r.data, want, "{sql}");
    }
    // …and the rows come in by the same grammar.
    ok(&mut c, "CREATE TABLE e (a int, b text)");
    let r = run(
        &mut c,
        "COPY e FROM STDIN (FORMAT csv, ESCAPE '\\')",
        "2,\"a\\\"b\"\n",
    );
    assert_eq!(r.error, None);
    let r = run(
        &mut c,
        "COPY e FROM STDIN CSV HEADER DELIMITER AS ';'",
        "a;b\n3;c\n",
    );
    assert_eq!(r.error, None);
    assert_eq!(
        rows(&mut c, "SELECT a, b FROM e ORDER BY a"),
        "2|a\"b\n3|c\n"
    );
}

#[test]
fn a_written_schema_is_final_for_copy() {
    let (_child, mut c) = server();
    ok(&mut c, "CREATE SCHEMA sa");
    ok(&mut c, "CREATE TABLE public.t (v text)");
    ok(&mut c, "CREATE TABLE sa.t (v text)");
    ok(&mut c, "INSERT INTO public.t VALUES ('public')");
    ok(&mut c, "INSERT INTO sa.t VALUES ('sa')");
    ok(&mut c, "SET search_path = sa, public");
    assert_eq!(run(&mut c, "COPY public.t TO STDOUT", "").data, "public\n");
    assert_eq!(run(&mut c, "COPY t TO STDOUT", "").data, "sa\n");
    let r = run(&mut c, "COPY public.t FROM STDIN", "into-public\n");
    assert_eq!(r.error, None);
    assert_eq!(
        rows(&mut c, "SELECT v FROM public.t ORDER BY v"),
        "into-public\npublic\n"
    );
    assert_eq!(rows(&mut c, "SELECT v FROM sa.t ORDER BY v"), "sa\n");
}

#[test]
fn a_quoted_name_keeps_its_case() {
    let (_child, mut c) = server();
    ok(&mut c, "CREATE TABLE \"MixedCase\" (\"V\" text)");
    let r = run(&mut c, "COPY \"MixedCase\" (\"V\") FROM STDIN", "m\n");
    assert_eq!(r.error, None);
    assert_eq!(r.tag.as_deref(), Some("COPY 1"));
    assert_eq!(run(&mut c, "COPY \"MixedCase\" TO STDOUT", "").data, "m\n");
}

#[test]
fn an_empty_line_is_a_row() {
    let (_child, mut c) = server();
    ok(&mut c, "CREATE TABLE e2 (v text)");
    let r = run(&mut c, "COPY e2 FROM STDIN", "x\n\ny\n");
    assert_eq!(r.tag.as_deref(), Some("COPY 3"));
    ok(&mut c, "CREATE TABLE e3 (v text)");
    let r = run(&mut c, "COPY e3 FROM STDIN (FORMAT csv)", "x\n\n\"\"\n");
    assert_eq!(r.tag.as_deref(), Some("COPY 3"));
    assert_eq!(
        rows(
            &mut c,
            "SELECT count(*) FILTER (WHERE v = ''), count(*) FILTER (WHERE v IS NULL) FROM e3"
        ),
        "1|1\n"
    );
    // A two-column table cannot read an empty line.
    ok(&mut c, "CREATE TABLE e4 (a int, b text)");
    let r = run(&mut c, "COPY e4 FROM STDIN", "1\tx\n\n");
    assert_eq!(
        r.error.as_deref(),
        Some("22P04: missing data for column \"b\"")
    );
}

#[test]
fn a_value_is_read_by_its_columns_type() {
    let (_child, mut c) = server();
    ok(&mut c, "CREATE TABLE n (a int, d text)");
    let r = run(&mut c, "COPY n FROM STDIN", "1.5\tx\n");
    assert_eq!(
        r.error.as_deref(),
        Some("22P02: invalid input syntax for type integer: \"1.5\"")
    );
    let r = run(&mut c, "COPY n FROM STDIN", "4\t+5\n");
    assert_eq!(r.error, None);
    assert_eq!(rows(&mut c, "SELECT a, d FROM n"), "4|+5\n");
}

#[test]
fn on_error_ignore_skips_what_a_column_cannot_read() {
    let (_child, mut c) = server();
    ok(&mut c, "CREATE TABLE ni (a int NOT NULL, b text)");
    let r = run(
        &mut c,
        "COPY ni FROM STDIN (ON_ERROR ignore, LOG_VERBOSITY verbose)",
        "1\tx\nzz\ty\n3\tz\n",
    );
    assert_eq!(r.error, None);
    assert_eq!(r.tag.as_deref(), Some("COPY 2"));
    assert_eq!(
        r.notices,
        [
            "skipping row due to data type incompatibility at line 2 for column \"a\": \"zz\"",
            "1 row was skipped due to data type incompatibility",
        ]
    );
    let r = run(
        &mut c,
        "COPY ni FROM STDIN (ON_ERROR ignore)",
        "qq\tx\nrr\ty\n",
    );
    assert_eq!(
        r.notices,
        ["2 rows were skipped due to data type incompatibility"]
    );
    let r = run(
        &mut c,
        "COPY ni FROM STDIN (ON_ERROR ignore, LOG_VERBOSITY silent)",
        "qq\tx\n",
    );
    assert!(r.notices.is_empty(), "{:?}", r.notices);
    // A value that converts but breaks a constraint still ends the COPY.
    let r = run(&mut c, "COPY ni FROM STDIN (ON_ERROR ignore)", "\\N\tx\n");
    assert!(
        r.error.as_deref().is_some_and(|e| e.starts_with("23502:")),
        "{:?}",
        r.error
    );
    let r = run(
        &mut c,
        "COPY ni FROM STDIN (ON_ERROR ignore, REJECT_LIMIT 1)",
        "q\tx\nr\ty\n",
    );
    assert_eq!(
        r.error.as_deref(),
        Some("22P02: skipped more than REJECT_LIMIT (1) rows due to data type incompatibility")
    );
    assert_eq!(rows(&mut c, "SELECT a FROM ni ORDER BY a"), "1\n3\n");
}

#[test]
fn header_match_checks_the_names() {
    let (_child, mut c) = server();
    ok(&mut c, "CREATE TABLE hm (a int, b text)");
    for (input, want) in [
        ("a,b\n1,x\n", None),
        (
            "a,c\n",
            Some("22P04: column name mismatch in header line field 2: got \"c\", expected \"b\""),
        ),
        (
            "a\n",
            Some("22P04: wrong number of fields in header line: got 1, expected 2"),
        ),
        (
            ",b\n",
            Some(
                "22P04: column name mismatch in header line field 1: got null value (\"\"), expected \"a\"",
            ),
        ),
    ] {
        let r = run(
            &mut c,
            "COPY hm FROM STDIN (FORMAT csv, HEADER match)",
            input,
        );
        assert_eq!(r.error.as_deref(), want, "{input:?}");
    }
    assert_eq!(rows(&mut c, "SELECT a, b FROM hm"), "1|x\n");
}

#[test]
fn a_misused_option_is_refused_in_postgresqls_words() {
    let (_child, mut c) = server();
    ok(&mut c, "CREATE TABLE sq (a int, b text)");
    for (sql, want) in [
        (
            "COPY sq TO STDOUT (HEADER, HEADER)",
            "42601: conflicting or redundant options",
        ),
        (
            "COPY sq TO STDOUT (ON_ERROR stop)",
            "22023: COPY ON_ERROR cannot be used with COPY TO",
        ),
        (
            "COPY sq TO STDOUT (HEADER match)",
            "0A000: cannot use \"match\" with HEADER in COPY TO",
        ),
        (
            "COPY sq FROM STDIN (REJECT_LIMIT 1)",
            "22023: COPY REJECT_LIMIT requires ON_ERROR to be set to IGNORE",
        ),
        (
            "COPY sq FROM STDIN (ON_ERROR bogus)",
            "22023: COPY ON_ERROR \"bogus\" not recognized",
        ),
        (
            "COPY sq (a) FROM STDIN (FORMAT csv, FORCE_NULL (b))",
            "42P10: FORCE_NULL column \"b\" not referenced by COPY",
        ),
        (
            "COPY sq FROM STDIN (FORMAT csv, FORCE_NULL (zz))",
            "42703: column \"zz\" of relation \"sq\" does not exist",
        ),
        (
            "COPY sq (a) TO STDOUT (FORMAT csv, FORCE_QUOTE (b))",
            "42P10: FORCE_QUOTE column \"b\" not referenced by COPY",
        ),
        (
            "COPY sq FROM STDIN (QUOTE 'x')",
            "0A000: COPY QUOTE requires CSV mode",
        ),
        (
            "COPY sq FROM STDIN (nosuch 1)",
            "42601: option \"nosuch\" not recognized",
        ),
    ] {
        assert_eq!(run(&mut c, sql, "").error.as_deref(), Some(want), "{sql}");
    }
}

#[test]
fn a_table_created_in_the_transaction_takes_a_copy() {
    let (_child, mut c) = server();
    ok(&mut c, "BEGIN");
    ok(&mut c, "CREATE TABLE tx1 (a int, b text)");
    let r = run(&mut c, "COPY tx1 (a, b) FROM stdin", "1\tx\n");
    assert_eq!(r.error, None);
    assert_eq!(r.tag.as_deref(), Some("COPY 1"));
    let r = run(&mut c, "COPY tx1 TO STDOUT", "");
    assert_eq!(r.error, None);
    assert_eq!(r.data, "1\tx\n");
    ok(&mut c, "COMMIT");
    assert_eq!(rows(&mut c, "SELECT a, b FROM tx1"), "1|x\n");
}

/// The engine's COPY TO — a file endpoint, the embedded host — rendered
/// `timestamptz` in UTC whatever the session's `TimeZone`: on the
/// published 9.0.2 a file written under `Asia/Tokyo` held
/// `2024-01-15 10:30:00+00`, where PostgreSQL 18.6 writes
/// `2024-01-15 19:30:00+09`, as the same server's COPY TO STDOUT did.
#[test]
fn a_file_is_written_in_the_sessions_time_zone() {
    let (_child, mut c, dir) = server_in();
    ok(&mut c, "CREATE TABLE tz (t timestamptz, f float8)");
    ok(
        &mut c,
        "INSERT INTO tz VALUES ('2024-01-15 10:30:00+00', 0.1)",
    );
    ok(&mut c, "SET TimeZone = 'Asia/Tokyo'");
    let path = dir.join("tz.out");
    let r = run(&mut c, &format!("COPY tz TO '{}'", path.display()), "");
    assert_eq!(r.error, None);
    let stdout = run(&mut c, "COPY tz TO STDOUT", "").data;
    assert_eq!(stdout, "2024-01-15 19:30:00+09\t0.1\n");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), stdout);
}

/// The query form takes what PostgreSQL takes: a WITH query and a VALUES
/// list as well as a SELECT.
#[test]
fn the_query_form_takes_with_and_values() {
    let (_child, mut c) = server();
    for (sql, want) in [
        (
            "COPY (WITH x AS (SELECT 1 AS n) SELECT n FROM x) TO STDOUT",
            "1\n",
        ),
        (
            "COPY (VALUES (1, 'a'), (2, NULL)) TO STDOUT",
            "1\ta\n2\t\\N\n",
        ),
        (
            "COPY (SELECT 1 AS n UNION ALL SELECT 2) TO STDOUT (FORMAT csv, HEADER)",
            "n\n1\n2\n",
        ),
    ] {
        let r = run(&mut c, sql, "");
        assert_eq!(r.error, None, "{sql}");
        assert_eq!(r.data, want, "{sql}");
    }
}
