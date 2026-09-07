//! v7.39 (read01 round 118, B3) — `SHOW transaction_isolation` reports the
//! live level over the wire. It used to be canned to "read committed" in the
//! pgwire layer, so `BEGIN ISOLATION LEVEL REPEATABLE READ; SHOW
//! transaction_isolation` wrongly reported "read committed". The canned
//! response is gone; the query now reaches the engine, which reads the live
//! `current_isolation_level` set by `BEGIN ISOLATION LEVEL …` and reverts to
//! the default at COMMIT / ROLLBACK. Verified over the real pgwire protocol.
//!
//! (Concurrent per-connection isolation is still gated on per-connection TxId —
//! a separate RFC. This test uses one connection.)

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn local_spawn(db: &std::path::Path) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .with_pgwire()
        .spawn()
}

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = crate::common::tmp_base().join(format!("spg-e2e-showiso-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

struct PgMessage {
    ty: u8,
    body: Vec<u8>,
}

fn read_message(s: &mut TcpStream) -> PgMessage {
    let mut header = [0u8; 5];
    s.read_exact(&mut header).expect("header");
    let ty = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let body_len = len.saturating_sub(4);
    let mut body = vec![0u8; body_len];
    if body_len > 0 {
        s.read_exact(&mut body).expect("body");
    }
    PgMessage { ty, body }
}

fn send_msg(s: &mut TcpStream, ty: u8, body: &[u8]) {
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.push(ty);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(body);
    s.write_all(&out).unwrap();
}

fn send_startup(s: &mut TcpStream, user: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608_u32.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.push(0);
    let total = u32::try_from(body.len() + 4).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    send_msg(s, b'Q', &body);
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

/// First text cell of the first DataRow (`D`) of `sql`'s reply.
fn first_cell(s: &mut TcpStream, sql: &str) -> String {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    let d = msgs
        .iter()
        .find(|m| m.ty == b'D')
        .unwrap_or_else(|| panic!("no DataRow for {sql}"));
    // DataRow: int16 field count, then per field int32 len + bytes.
    let body = &d.body;
    let n = u16::from_be_bytes([body[0], body[1]]);
    assert!(n >= 1, "empty DataRow for {sql}");
    let len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
    assert!(len >= 0, "NULL cell for {sql}");
    let start = 6;
    let end = start + len as usize;
    String::from_utf8_lossy(&body[start..end]).into_owned()
}

fn run_ok(s: &mut TcpStream, sql: &str) {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    assert!(
        msgs.iter().all(|m| m.ty != b'E'),
        "unexpected error for {sql}"
    );
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "admin");
    let _ = read_until_ready(&mut s);
    s
}

#[test]
fn show_transaction_isolation_reports_live_level() {
    let dir = unique_tmpdir("live");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    assert_eq!(
        first_cell(&mut s, "SHOW transaction_isolation"),
        "read committed"
    );

    run_ok(&mut s, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(
        first_cell(&mut s, "SHOW transaction_isolation"),
        "repeatable read"
    );
    run_ok(&mut s, "COMMIT");
    // Reverts to the default at transaction end.
    assert_eq!(
        first_cell(&mut s, "SHOW transaction_isolation"),
        "read committed"
    );

    run_ok(&mut s, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    assert_eq!(
        first_cell(&mut s, "SHOW transaction_isolation"),
        "serializable"
    );
    // PG's multi-word spelling reports the live level too.
    assert_eq!(
        first_cell(&mut s, "SHOW TRANSACTION ISOLATION LEVEL"),
        "serializable"
    );
    run_ok(&mut s, "ROLLBACK");
    assert_eq!(
        first_cell(&mut s, "SHOW transaction_isolation"),
        "read committed"
    );
    assert_eq!(
        first_cell(&mut s, "SHOW TRANSACTION ISOLATION LEVEL"),
        "read committed"
    );
}

/// Count the rows a query returns over the wire.
fn row_count(s: &mut TcpStream, sql: &str) -> usize {
    send_query(s, sql);
    read_until_ready(s).iter().filter(|m| m.ty == b'D').count()
}

/// The SQLSTATE of the ErrorResponse, or None when the statement was accepted.
fn err_code(s: &mut TcpStream, sql: &str) -> Option<String> {
    send_query(s, sql);
    let msgs = read_until_ready(s);
    let e = msgs.iter().find(|m| m.ty == b'E')?;
    // ErrorResponse: NUL-terminated fields, each prefixed by a type byte;
    // 'C' carries the SQLSTATE.
    let mut i = 0usize;
    while i < e.body.len() && e.body[i] != 0 {
        let field = e.body[i];
        let start = i + 1;
        let mut end = start;
        while end < e.body.len() && e.body[end] != 0 {
            end += 1;
        }
        if field == b'C' {
            return Some(String::from_utf8_lossy(&e.body[start..end]).into_owned());
        }
        i = end + 1;
    }
    None
}

/// v7.40.12 — round 118 removed ONE canned answer and left the shortcut that
/// produced it. The shortcut answered every `SHOW` from a copy: the engine's
/// stored session_params first, then a 21-entry table of frozen defaults. A
/// copy cannot carry a DERIVED value, so four more answers were wrong, and the
/// engine had all four right on the same connection. Measured against
/// PostgreSQL 18.6; every expectation below is PG's own answer.
///
/// The corpus has pinned three of these since v7.39 — but only through the
/// perm-runner's wire legs, which run in the `full` tier. They are here so the
/// e2e gate, which runs on every commit, asks the same questions.
#[test]
fn show_answers_derived_values_not_a_stale_copy() {
    let dir = unique_tmpdir("derived");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    // (1) `transaction_read_only` inside a read-only block. PG: on.
    // The shortcut answered `off` from the frozen table while
    // `current_setting('transaction_read_only')` — same connection, same
    // transaction, the engine's own reading — answered `on`.
    assert_eq!(first_cell(&mut s, "SHOW transaction_read_only"), "off");
    run_ok(&mut s, "BEGIN READ ONLY");
    assert_eq!(first_cell(&mut s, "SHOW transaction_read_only"), "on");
    assert_eq!(
        first_cell(&mut s, "SELECT current_setting('transaction_read_only')"),
        "on",
        "the two surfaces must not disagree — that disagreement is what named this"
    );
    run_ok(&mut s, "ROLLBACK");
    assert_eq!(first_cell(&mut s, "SHOW transaction_read_only"), "off");

    // (2) `transaction_isolation` after `SET default_transaction_isolation`.
    // The stored copy is written by BEGIN, not by SET, so SHOW answered one
    // statement behind: `read committed` here, then `repeatable read` after
    // it had been set back.
    run_ok(
        &mut s,
        "SET default_transaction_isolation = 'repeatable read'",
    );
    assert_eq!(
        first_cell(&mut s, "SHOW transaction_isolation"),
        "repeatable read"
    );
    run_ok(
        &mut s,
        "SET default_transaction_isolation = 'read committed'",
    );
    assert_eq!(
        first_cell(&mut s, "SHOW transaction_isolation"),
        "read committed"
    );

    // (3) `SHOW ALL`. PG 18.6 returns 399 rows; the engine returns 399 and
    // `SELECT count(*) FROM pg_settings` answered 399 on the connection that
    // saw the shortcut's 21.
    let all = row_count(&mut s, "SHOW ALL");
    let settings = first_cell(&mut s, "SELECT count(*) FROM pg_settings");
    assert_eq!(
        all.to_string(),
        settings,
        "SHOW ALL and pg_settings are one inventory; the shortcut had its own"
    );
    assert!(
        all > 300,
        "SHOW ALL returned {all} rows — the wire's 21-entry table is back"
    );

    // (4) PG's two-word spelling of the timezone GUC. Only the shortcut knew
    // it; with the shortcut gone the parser has to.
    assert_eq!(first_cell(&mut s, "SHOW TIME ZONE"), "UTC");
    assert_eq!(first_cell(&mut s, "SHOW timezone"), "UTC");

    // (5) PG's spelling of the login identity, both ways. It has no
    // `pg_settings` row in PG either, so `SHOW ALL` above stays at PG's
    // count; `SELECT session_user` already answered on this connection
    // while `SHOW session_authorization` denied the name existed.
    let who = first_cell(&mut s, "SELECT session_user");
    assert_eq!(first_cell(&mut s, "SHOW session_authorization"), who);
    assert_eq!(first_cell(&mut s, "SHOW SESSION AUTHORIZATION"), who);

    // An unknown name is still PG's error, not an empty row.
    assert_eq!(err_code(&mut s, "SHOW spam_x").as_deref(), Some("42704"));
}

/// v7.40.12 — `SELECT 1` inside a transaction block is a query, and PG counts
/// it: the next `SET TRANSACTION ISOLATION LEVEL` is refused with 25001.
/// SPG answered pure-integer selects from a wire fast path that never reached
/// the engine, so the transaction's statement counter never moved and the
/// switch was accepted. `SELECT * FROM t` in the same position refused
/// correctly, which is what named the shortcut.
///
/// Measured on PostgreSQL 18.6: the isolation level is refused after the
/// first query, `SET TRANSACTION READ ONLY` is not.
#[test]
fn an_integer_select_inside_a_block_is_a_query() {
    let dir = unique_tmpdir("intselect");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    run_ok(&mut s, "BEGIN");
    assert_eq!(first_cell(&mut s, "SELECT 1"), "1");
    assert_eq!(
        err_code(&mut s, "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE").as_deref(),
        Some("25001"),
        "the fast path answered SELECT 1 without telling the engine a query ran"
    );
    run_ok(&mut s, "ROLLBACK");

    // Before the first query it is still allowed.
    run_ok(&mut s, "BEGIN");
    run_ok(&mut s, "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE");
    assert_eq!(
        first_cell(&mut s, "SHOW transaction_isolation"),
        "serializable"
    );
    run_ok(&mut s, "ROLLBACK");

    // The read/write half is a DIFFERENT rule, and the refusal used to
    // cover the whole statement. Measured on PG 18.6, after a query:
    // tightening is always allowed, and only LOOSENING a read-only
    // transaction is refused — with its own wording, and 25001 too.
    run_ok(&mut s, "BEGIN");
    assert_eq!(first_cell(&mut s, "SELECT 1"), "1");
    run_ok(&mut s, "SET TRANSACTION READ ONLY"); // tighten: allowed
    run_ok(&mut s, "ROLLBACK");

    run_ok(&mut s, "BEGIN");
    assert_eq!(first_cell(&mut s, "SELECT 1"), "1");
    run_ok(&mut s, "SET TRANSACTION READ WRITE"); // no change: allowed
    run_ok(&mut s, "ROLLBACK");

    run_ok(&mut s, "BEGIN READ ONLY");
    run_ok(&mut s, "SET TRANSACTION READ WRITE"); // before any query: allowed
    run_ok(&mut s, "ROLLBACK");

    run_ok(&mut s, "BEGIN READ ONLY");
    assert_eq!(first_cell(&mut s, "SELECT 1"), "1");
    assert_eq!(
        err_code(&mut s, "SET TRANSACTION READ WRITE").as_deref(),
        Some("25001"),
        "loosening a read-only transaction after a query is PG's 25001"
    );
    run_ok(&mut s, "ROLLBACK");

    // DEFERRABLE is the third of the clause and was parsed and dropped,
    // the way READ ONLY was before v7.39. Measured on PG 18.6: `off` by
    // default, `on` inside `BEGIN DEFERRABLE`, `off` again afterwards,
    // and 25001 after a query — in BOTH directions, unlike read/write.
    assert_eq!(first_cell(&mut s, "SHOW transaction_deferrable"), "off");
    run_ok(&mut s, "BEGIN DEFERRABLE");
    assert_eq!(first_cell(&mut s, "SHOW transaction_deferrable"), "on");
    run_ok(&mut s, "ROLLBACK");
    assert_eq!(first_cell(&mut s, "SHOW transaction_deferrable"), "off");

    run_ok(&mut s, "BEGIN");
    run_ok(&mut s, "SET TRANSACTION DEFERRABLE");
    assert_eq!(first_cell(&mut s, "SHOW transaction_deferrable"), "on");
    run_ok(&mut s, "ROLLBACK");

    run_ok(&mut s, "BEGIN");
    assert_eq!(first_cell(&mut s, "SELECT 1"), "1");
    assert_eq!(
        err_code(&mut s, "SET TRANSACTION DEFERRABLE").as_deref(),
        Some("25001")
    );
    run_ok(&mut s, "ROLLBACK");
    run_ok(&mut s, "BEGIN");
    assert_eq!(first_cell(&mut s, "SELECT 1"), "1");
    assert_eq!(
        err_code(&mut s, "SET TRANSACTION NOT DEFERRABLE").as_deref(),
        Some("25001")
    );
    run_ok(&mut s, "ROLLBACK");

    // Outside a block the fast path still answers, which is its whole
    // population: pool keepalives are sent in autocommit.
    assert_eq!(first_cell(&mut s, "SELECT 1"), "1");
    assert_eq!(first_cell(&mut s, "SELECT -42"), "-42");
}

/// v7.40.12 — which statements make a transaction "have run a query".
///
/// PG refuses `SET TRANSACTION ISOLATION LEVEL` once the transaction has
/// taken a snapshot, and a UTILITY statement does not take one. SPG's
/// counter was bumped by every statement instead. It was invisible while
/// the pgwire shortcut answered `SHOW` without the engine; removing that
/// shortcut made `BEGIN; SHOW work_mem; SET TRANSACTION …` refuse where
/// PG answers `SET` — and `SET` had been over-counted the whole time,
/// since it always reached the engine.
///
/// Every row below was run against a live PostgreSQL 18.6 in exactly
/// this shape (`BEGIN; <statement>; SET TRANSACTION ISOLATION LEVEL
/// SERIALIZABLE`) and carries PG's answer, including the ones that look
/// like plumbing and still count.
#[test]
fn only_a_snapshot_makes_a_transaction_have_run_a_query() {
    let dir = unique_tmpdir("snapshot");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    run_ok(&mut s, "CREATE TABLE ro (x INT)");

    // Does NOT count — the switch still succeeds after each of these.
    for stmt in [
        "SET application_name = 'x'",
        "SET LOCAL work_mem = '8MB'",
        "SHOW work_mem",
        "RESET application_name",
        "SAVEPOINT sp",
        "LOCK TABLE ro",
        "LISTEN ch",
        "UNLISTEN ch",
        "NOTIFY ch",
    ] {
        run_ok(&mut s, "BEGIN");
        run_ok(&mut s, stmt);
        // SAVEPOINT opens a subtransaction, where PG refuses the switch
        // for a DIFFERENT reason and with different words — that rule is
        // pinned below. Here only the counter is under test.
        let code = err_code(&mut s, "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE");
        if stmt == "SAVEPOINT sp" {
            assert_eq!(code.as_deref(), Some("25001"), "after {stmt}");
        } else {
            assert_eq!(code, None, "`{stmt}` must not count as a query");
        }
        run_ok(&mut s, "ROLLBACK");
    }

    // Counts. `DISCARD PLANS`, `DEALLOCATE ALL`, `COMMENT ON`, `PREPARE`
    // and `EXPLAIN` are in this half, measured — "utility statement" is
    // not the dividing line, taking a snapshot is.
    for stmt in [
        "SELECT 1",
        "SELECT * FROM ro",
        "INSERT INTO ro VALUES (1)",
        "CREATE TABLE zz (a INT)",
        "TRUNCATE ro",
        "EXPLAIN SELECT 1",
        "COMMENT ON TABLE ro IS 'x'",
        "PREPARE p1 AS SELECT 1",
        "DEALLOCATE ALL",
        "DISCARD PLANS",
    ] {
        run_ok(&mut s, "BEGIN");
        run_ok(&mut s, stmt);
        assert_eq!(
            err_code(&mut s, "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE").as_deref(),
            Some("25001"),
            "`{stmt}` must count as a query"
        );
        run_ok(&mut s, "ROLLBACK");
    }
}

/// v7.40.12 — inside a SUBTRANSACTION, PG refuses two of the three
/// halves of `SET TRANSACTION` whatever the snapshot says, each with its
/// own wording and all three 25001. Measured on PG 18.6.
///
/// The savepoint has to be OPEN: after `RELEASE SAVEPOINT` the switch is
/// accepted again, and after `ROLLBACK TO SAVEPOINT` it is not, because
/// PG keeps the savepoint there.
#[test]
fn a_subtransaction_refuses_the_isolation_switch() {
    let dir = unique_tmpdir("subtx");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    run_ok(&mut s, "BEGIN");
    run_ok(&mut s, "SAVEPOINT sp");
    assert_eq!(
        err_code(&mut s, "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE").as_deref(),
        Some("25001")
    );
    run_ok(&mut s, "ROLLBACK");

    run_ok(&mut s, "BEGIN");
    run_ok(&mut s, "SAVEPOINT sp");
    assert_eq!(
        err_code(&mut s, "SET TRANSACTION DEFERRABLE").as_deref(),
        Some("25001")
    );
    run_ok(&mut s, "ROLLBACK");

    // READ ONLY is accepted in a subtransaction — tightening always is.
    run_ok(&mut s, "BEGIN");
    run_ok(&mut s, "SAVEPOINT sp");
    run_ok(&mut s, "SET TRANSACTION READ ONLY");
    run_ok(&mut s, "ROLLBACK");

    // Loosening is not, and PG words this one differently again.
    run_ok(&mut s, "BEGIN READ ONLY");
    run_ok(&mut s, "SAVEPOINT sp");
    assert_eq!(
        err_code(&mut s, "SET TRANSACTION READ WRITE").as_deref(),
        Some("25001")
    );
    run_ok(&mut s, "ROLLBACK");

    // RELEASE ends the subtransaction; ROLLBACK TO does not.
    run_ok(&mut s, "BEGIN");
    run_ok(&mut s, "SAVEPOINT sp");
    run_ok(&mut s, "RELEASE SAVEPOINT sp");
    run_ok(&mut s, "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE");
    run_ok(&mut s, "ROLLBACK");

    run_ok(&mut s, "BEGIN");
    run_ok(&mut s, "SAVEPOINT sp");
    run_ok(&mut s, "ROLLBACK TO SAVEPOINT sp");
    assert_eq!(
        err_code(&mut s, "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE").as_deref(),
        Some("25001")
    );
    run_ok(&mut s, "ROLLBACK");
}

/// Run a whole multi-statement script as ONE simple query — the shape
/// `psql -c "a; b; c"` sends, and the one the pgwire multi-statement
/// path wraps in a transaction of its own.
fn run_script(s: &mut TcpStream, sql: &str) -> Vec<String> {
    send_query(s, sql);
    read_until_ready(s)
        .iter()
        .filter_map(|m| match m.ty {
            b'C' | b'E' => Some(
                String::from_utf8_lossy(&m.body)
                    .replace('\0', " ")
                    .trim()
                    .to_string(),
            ),
            _ => None,
        })
        .collect()
}

/// v7.40.12 — `BEGIN <modes>` on an already-open transaction applies the
/// modes. It used to warn and throw them away.
///
/// This is the safety-critical face of it: the pgwire multi-statement
/// path wraps every script in a transaction, so EVERY leading `BEGIN` in
/// a script is a nested one. `BEGIN READ ONLY; INSERT …; COMMIT;` sent
/// as one script therefore opened nothing read-only, ACCEPTED the write
/// and committed it. PG 18.6 refuses it and the table stays empty —
/// measured on both engines with the same one-line script.
///
/// Applications open read-only transactions as a safety measure, which
/// is why the corpus file that pins the single-statement form says
/// accepting the write is the worst available answer.
#[test]
fn a_begin_inside_a_transaction_still_carries_its_modes() {
    let dir = unique_tmpdir("nestedbegin");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());
    run_ok(&mut s, "CREATE TABLE roro (x INT)");

    // The headline. One script, one round trip.
    let out = run_script(
        &mut s,
        "BEGIN READ ONLY; INSERT INTO roro VALUES (1); COMMIT;",
    );
    assert!(
        out.iter()
            .any(|m| m.contains("cannot execute INSERT in a read-only transaction")),
        "the write was not refused: {out:?}"
    );
    assert_eq!(first_cell(&mut s, "SELECT count(*) FROM roro"), "0");

    // The modes arrive wherever the BEGIN sits in the script, which is
    // PG's behaviour too — its implicit block is converted in place.
    let out = run_script(
        &mut s,
        "SELECT 1; BEGIN READ ONLY; INSERT INTO roro VALUES (2); COMMIT;",
    );
    assert!(
        out.iter()
            .any(|m| m.contains("cannot execute INSERT in a read-only transaction")),
        "a BEGIN after a statement lost its modes: {out:?}"
    );
    assert_eq!(first_cell(&mut s, "SELECT count(*) FROM roro"), "0");

    // And the refusals come with the modes: PG reports SET TRANSACTION's
    // own message for a statement the user spelled BEGIN.
    let out = run_script(
        &mut s,
        "SELECT 1; BEGIN ISOLATION LEVEL SERIALIZABLE; SELECT 1;",
    );
    assert!(
        out.iter()
            .any(|m| m.contains("SET TRANSACTION ISOLATION LEVEL must be called before any query")),
        "{out:?}"
    );

    // Nested inside an EXPLICIT block, one statement per round trip: PG
    // warns AND applies. The warning is a NoticeResponse, not an error.
    run_ok(&mut s, "BEGIN");
    assert_eq!(first_cell(&mut s, "SELECT 1"), "1");
    assert_eq!(first_cell(&mut s, "SHOW transaction_read_only"), "off");
    run_ok(&mut s, "BEGIN READ ONLY");
    assert_eq!(
        first_cell(&mut s, "SHOW transaction_read_only"),
        "on",
        "the nested BEGIN warned and threw its modes away"
    );
    run_ok(&mut s, "ROLLBACK");

    // PG's tag for this statement is both words; the default arm
    // answered `START`.
    let out = run_script(&mut s, "START TRANSACTION READ ONLY; COMMIT;");
    assert!(
        out.iter().any(|m| m.contains("START TRANSACTION")),
        "command tag: {out:?}"
    );
}

/// v7.40.12 — `SERIALIZABLE READ ONLY DEFERRABLE` waits for a snapshot
/// it can run against without serialization failures, which means
/// waiting for every concurrent SERIALIZABLE read-write transaction to
/// finish. Until now SPG carried the word and did nothing with it.
///
/// Measured on PG 18.6 with the same script on both engines: with a
/// serializable writer open, the reader's first `SELECT` took 2,996 ms
/// with DEFERRABLE and 0.9 ms with NOT DEFERRABLE, and `BEGIN` was
/// instant in both — the wait is at the first snapshot, not at BEGIN.
/// The wait is cancelled by `statement_timeout` and NOT by
/// `lock_timeout`; both measured, both pinned below.
///
/// Every assertion here is bounded by a timeout on purpose: a pin for a
/// wait must FAIL when the wait is wrong, never hang.
#[test]
fn a_deferrable_reader_waits_for_a_safe_snapshot() {
    let dir = unique_tmpdir("deferrable");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let addr = addrs.pgwire.as_ref().unwrap();

    let mut setup = open(addr);
    run_ok(&mut setup, "CREATE TABLE dfr (id INT PRIMARY KEY, v INT)");
    run_ok(&mut setup, "INSERT INTO dfr VALUES (1, 10), (2, 20)");

    // A serializable WRITER, left open.
    let mut writer = open(addr);
    run_ok(&mut writer, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run_ok(&mut writer, "UPDATE dfr SET v = v + 1 WHERE id = 1");

    // NOT DEFERRABLE does not wait: it answers inside its own timeout.
    let mut plain = open(addr);
    run_ok(&mut plain, "SET statement_timeout = '4s'");
    run_ok(
        &mut plain,
        "BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY NOT DEFERRABLE",
    );
    assert_eq!(first_cell(&mut plain, "SELECT sum(v) FROM dfr"), "30");
    run_ok(&mut plain, "COMMIT");

    // DEFERRABLE waits, and the only thing that ends the wait here is
    // the statement timeout — the writer is still open.
    let mut deferred = open(addr);
    run_ok(&mut deferred, "SET statement_timeout = '700ms'");
    run_ok(
        &mut deferred,
        "BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY DEFERRABLE",
    );
    let started = std::time::Instant::now();
    let code = err_code(&mut deferred, "SELECT sum(v) FROM dfr");
    let waited = started.elapsed();
    assert!(
        code.is_some(),
        "the deferrable reader did not wait: it answered in {waited:?}"
    );
    assert!(
        waited >= std::time::Duration::from_millis(500),
        "it returned in {waited:?}, too fast to have waited for the timeout"
    );
    run_ok(&mut deferred, "ROLLBACK");

    // lock_timeout does NOT end this wait — measured on PG 18.6. With a
    // short lock_timeout and a longer statement_timeout, the statement
    // timeout is the one that fires.
    let mut both = open(addr);
    run_ok(&mut both, "SET lock_timeout = '300ms'");
    run_ok(&mut both, "SET statement_timeout = '1200ms'");
    run_ok(
        &mut both,
        "BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY DEFERRABLE",
    );
    let started = std::time::Instant::now();
    assert!(err_code(&mut both, "SELECT sum(v) FROM dfr").is_some());
    let waited = started.elapsed();
    assert!(
        waited >= std::time::Duration::from_millis(900),
        "lock_timeout ended the wait at {waited:?}; only statement_timeout may"
    );
    run_ok(&mut both, "ROLLBACK");

    // Once the writer is gone the wait ends and the reader proceeds.
    run_ok(&mut writer, "COMMIT");
    let mut after = open(addr);
    run_ok(&mut after, "SET statement_timeout = '4s'");
    run_ok(
        &mut after,
        "BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY DEFERRABLE",
    );
    let started = std::time::Instant::now();
    assert_eq!(first_cell(&mut after, "SELECT sum(v) FROM dfr"), "31");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "nothing was open to wait for, yet it waited"
    );
    run_ok(&mut after, "COMMIT");

    // A read-WRITE serializable transaction never defers, whatever the
    // word says: PG only defers a READ ONLY one.
    run_ok(&mut writer, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run_ok(&mut writer, "UPDATE dfr SET v = v + 1 WHERE id = 2");
    let mut rw = open(addr);
    run_ok(&mut rw, "SET statement_timeout = '4s'");
    run_ok(&mut rw, "BEGIN ISOLATION LEVEL SERIALIZABLE DEFERRABLE");
    assert_eq!(first_cell(&mut rw, "SELECT sum(v) FROM dfr"), "31");
    run_ok(&mut rw, "COMMIT");
    run_ok(&mut writer, "ROLLBACK");
}
