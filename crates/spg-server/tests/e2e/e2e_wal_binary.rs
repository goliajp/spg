//! v4.41 — verify the v3 WAL record path.
//!
//! Three guarantees the binary-WAL ship has to hold:
//!
//!   1. Auto-commit writes emit **one** v3 record (vs. three v2
//!      records for the v4.34 `[BEGIN, sql, COMMIT]` block).
//!   2. A WAL containing v3 records (alone, or interleaved with
//!      v1/v2 records from an upgraded WAL file) replays into the
//!      same engine state as the writer.
//!   3. An unknown v3 type byte is fatal — replay refuses to skip
//!      it silently. This is the forward-compat fence: future
//!      type tags must bump the version or the binary won't load.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use spg_wire::{Frame, Op, build_query, encode, parse_command_complete};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(15);

const WAL_V2_SENTINEL: u32 = 0x8000_0000;
const WAL_V3_FLAG: u32 = 0x4000_0000;
const WAL_V3_TYPE_AUTO_COMMIT_SQL: u8 = 0x01;

static TMPDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tmpdir(tag: &str) -> PathBuf {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let serial = TMPDIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir =
        crate::common::tmp_base().join(format!("spg-wal-binary-{tag}-{pid}-{nanos}-{serial}"));
    fs::create_dir_all(&dir).expect("create tmpdir");
    dir
}

/// Spawn an spg-server bound to an OS-chosen port and return both the
/// child + the actual bind address parsed from the server's "listening
/// on 127.0.0.1:PORT" stderr line.
///
/// **Why not `TcpListener::bind("127.0.0.1:0"); drop; pass port`?** That
/// pattern is racy: between dropping the probe and the child calling
/// `bind`, a parallel test's probe + drop + spawn cycle can grab the
/// same port (the kernel hands the freed port back to the ephemeral
/// pool). Two children then both try to bind, one fails; or worse, a
/// client connects to the wrong child's listener and the protocol
/// state diverges (manifests as `read header: Connection reset`).
///
/// Passing `127.0.0.1:0` directly to the child lets the kernel atomic-
/// ally allocate + reserve the port for that child's lifetime. The
/// child prints the bound address to stderr; we tail until we see it.
fn spawn_server_on_ephemeral_port(db: &Path, wal: &Path) -> (Child, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_spg-server"))
        .arg("127.0.0.1:0")
        .arg(db)
        .arg("-")
        .arg(wal)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD")
        .env_remove("SPG_PG_ADDR")
        .spawn()
        .expect("spawn spg-server");
    let stderr = child.stderr.take().expect("piped stderr");
    let addr = read_listening_addr(&mut child, stderr);
    (child, addr)
}

/// Read the child's stderr until a `spg-server: listening on 127.0.0.1:PORT`
/// line appears, then return the address. Remaining stderr is drained
/// off-thread so the pipe doesn't backpressure the server.
fn read_listening_addr(child: &mut Child, stderr: ChildStderr) -> String {
    let mut reader = BufReader::new(stderr);
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut line = String::new();
    while Instant::now() < deadline {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                if let Ok(Some(status)) = child.try_wait() {
                    panic!("server exited before printing listen addr: {status:?}");
                }
                thread::sleep(Duration::from_millis(20));
            }
            Ok(_) => {
                if let Some(addr) = extract_listen_addr(&line) {
                    thread::spawn(move || {
                        let mut sink = String::new();
                        let _ = BufReader::new(reader).read_to_string(&mut sink);
                    });
                    return addr;
                }
            }
            Err(e) => panic!("read stderr: {e}"),
        }
    }
    let _ = child.kill();
    panic!("server didn't print listen addr within {STARTUP_TIMEOUT:?}");
}

/// Parse `spg-server: listening on 127.0.0.1:38291` (auth-msg suffix
/// optional) → `"127.0.0.1:38291"`. None if the line shape doesn't
/// match — caller keeps reading.
fn extract_listen_addr(line: &str) -> Option<String> {
    let after = line.find("listening on ")?;
    let tail = &line[after + "listening on ".len()..];
    // tail might be "127.0.0.1:38291\n" or "127.0.0.1:38291 (auth msg)\n"
    let end = tail.find([' ', '\n', '\r']).unwrap_or(tail.len());
    Some(tail[..end].to_string())
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Connect to the (already-known-bound) server address with a short
/// retry window — the listener is up by the time stderr printed but
/// the OS may need a tick to accept(2). No race against parallel
/// tests since the port was allocated by the server itself.
fn connect_to(addr: &str) -> TcpStream {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match TcpStream::connect(addr) {
            Ok(s) => return s,
            Err(e) => {
                assert!(Instant::now() < deadline, "connect {addr}: {e}");
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Spawn an spg-server that is **expected to exit early** during
/// startup (e.g. replay-failure tests). Does not wait for a listen
/// line — the caller polls `try_wait` and asserts a non-zero exit.
fn spawn_server_expecting_replay_failure(db: &Path, wal: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_spg-server"))
        .arg("127.0.0.1:0")
        .arg(db)
        .arg("-")
        .arg(wal)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD")
        .env_remove("SPG_PG_ADDR")
        .spawn()
        .expect("spawn spg-server")
}

fn send_query(stream: &mut TcpStream, sql: &str) {
    let mut out = Vec::new();
    encode(&build_query(sql), &mut out).unwrap();
    stream.write_all(&out).unwrap();
}

fn read_frame(stream: &mut TcpStream) -> Frame {
    let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
    stream.read_exact(&mut header).expect("read header");
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).expect("known op");
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        stream.read_exact(&mut payload).expect("read payload");
    }
    Frame { op, payload }
}

fn expect_cc(stream: &mut TcpStream) {
    let f = read_frame(stream);
    if f.op != Op::CommandComplete {
        let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
        panic!("expected CC, got {:?}: {msg}", f.op);
    }
    parse_command_complete(&f).unwrap();
}

fn count_select_rows(stream: &mut TcpStream, sql: &str) -> usize {
    send_query(stream, sql);
    assert_eq!(read_frame(stream).op, Op::RowDescription);
    let mut count = 0;
    loop {
        let f = read_frame(stream);
        match f.op {
            Op::DataRow => count += 1,
            Op::DataRowBatch => count += spg_wire::parse_data_row_batch(&f).unwrap().len(),
            Op::CommandComplete => return count,
            other => panic!("unexpected: {other:?}"),
        }
    }
}

/// Walk a WAL byte stream and tally records by sentinel-bit version.
/// Mirrors the dispatch in `replay_wal_bytes` but only counts — used
/// to assert that v4.41 emits v3 records for auto-commit writes.
fn count_record_versions(bytes: &[u8]) -> (u32, u32, u32) {
    let (mut v1, mut v2, mut v3) = (0u32, 0u32, 0u32);
    let mut cur = 0;
    while cur + 4 <= bytes.len() {
        let raw_len = u32::from_le_bytes(bytes[cur..cur + 4].try_into().unwrap());
        cur += 4;
        let is_v2 = raw_len & WAL_V2_SENTINEL != 0;
        let is_v3 = is_v2 && raw_len & WAL_V3_FLAG != 0;
        let len_mask = if is_v3 {
            !(WAL_V2_SENTINEL | WAL_V3_FLAG)
        } else {
            !WAL_V2_SENTINEL
        };
        let len = (raw_len & len_mask) as usize;
        let header_after = if is_v3 {
            5
        } else if is_v2 {
            4
        } else {
            0
        };
        if cur + header_after + len > bytes.len() {
            break;
        }
        cur += header_after + len;
        if is_v3 {
            v3 += 1;
        } else if is_v2 {
            v2 += 1;
        } else {
            v1 += 1;
        }
    }
    (v1, v2, v3)
}

#[test]
fn auto_commit_write_emits_single_v3_record() {
    let dir = unique_tmpdir("emits-v3");
    let db = dir.join("spg.db");
    let wal = dir.join("wal.log");

    {
        let (raw_child, addr) = spawn_server_on_ephemeral_port(&db, &wal);
        let _child = ChildGuard(raw_child);
        let mut s = connect_to(&addr);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();

        // Three auto-commit writes: one DDL + two DML. v4.34 would
        // have written 9 records (3 per write); v4.41 writes 3.
        for sql in [
            "CREATE TABLE t (v INT NOT NULL)",
            "INSERT INTO t VALUES (1)",
            "INSERT INTO t VALUES (2)",
        ] {
            send_query(&mut s, sql);
            expect_cc(&mut s);
        }
    }

    let bytes = fs::read(&wal).expect("WAL file");
    let (v1, v2, v3) = count_record_versions(&bytes);
    assert_eq!(
        (v1, v2, v3),
        (0, 0, 3),
        "v4.41 must emit exactly 3 v3 records (1 per auto-commit write), got v1={v1} v2={v2} v3={v3}, total bytes={}",
        bytes.len()
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn v3_wal_replays_into_matching_engine_state() {
    let dir = unique_tmpdir("v3-replays");
    let db = dir.join("spg.db");
    let wal = dir.join("wal.log");

    {
        let (raw_child, addr) = spawn_server_on_ephemeral_port(&db, &wal);
        let _child = ChildGuard(raw_child);
        let mut s = connect_to(&addr);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        send_query(&mut s, "CREATE TABLE t (id INT, name TEXT)");
        expect_cc(&mut s);
        for (i, name) in [(1, "alice"), (2, "bob"), (3, "carol")] {
            send_query(&mut s, &format!("INSERT INTO t VALUES ({i}, '{name}')"));
            expect_cc(&mut s);
        }
    }

    let (raw_child, addr) = spawn_server_on_ephemeral_port(&db, &wal);
    let _child = ChildGuard(raw_child);
    let mut s = connect_to(&addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    assert_eq!(count_select_rows(&mut s, "SELECT * FROM t"), 3);
    fs::remove_dir_all(&dir).ok();
}

/// Forward-compat fence: a future-version type byte must not be
/// silently skipped during replay. Append a hand-crafted v3 record
/// with a reserved type byte to a real WAL and assert the server
/// refuses to start up clean.
#[test]
fn unknown_v3_type_byte_aborts_replay() {
    let dir = unique_tmpdir("v3-unknown-type");
    let db = dir.join("spg.db");
    let wal = dir.join("wal.log");

    // Phase 1: write a valid record so the WAL isn't empty.
    {
        let (raw_child, addr) = spawn_server_on_ephemeral_port(&db, &wal);
        let _child = ChildGuard(raw_child);
        let mut s = connect_to(&addr);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        send_query(&mut s, "CREATE TABLE t (v INT)");
        expect_cc(&mut s);
    }

    // Phase 2: append a hand-crafted v3 record with type byte 0xFE
    // (not WAL_V3_TYPE_AUTO_COMMIT_SQL).
    let payload = b"garbage";
    let mut bad_record = Vec::new();
    let len = payload.len() as u32;
    bad_record.extend_from_slice(&(len | WAL_V2_SENTINEL | WAL_V3_FLAG).to_le_bytes());
    // CRC covers [type || payload] — emit a valid CRC so the
    // replay reaches the type-byte dispatch (not the CRC check).
    let mut crc_input = Vec::with_capacity(1 + payload.len());
    crc_input.push(0xFE);
    crc_input.extend_from_slice(payload);
    let crc = spg_crypto::crc32::crc32(&crc_input);
    bad_record.extend_from_slice(&crc.to_le_bytes());
    bad_record.push(0xFE);
    bad_record.extend_from_slice(payload);
    {
        let mut f = OpenOptions::new()
            .append(true)
            .open(&wal)
            .expect("open WAL for append");
        f.write_all(&bad_record).expect("append bad record");
        f.sync_data().expect("fsync WAL");
    }

    // Phase 3: server must exit non-zero (replay refuses).
    let mut child = spawn_server_expecting_replay_failure(&db, &wal);
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut got_status = None;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            got_status = Some(status);
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    if got_status.is_none() {
        let _ = child.kill();
        panic!("server did not exit after unknown v3 type — should have refused replay");
    }
    let status = got_status.unwrap();
    assert!(
        !status.success(),
        "server exited 0 despite unknown v3 type byte ({status:?})"
    );
    let _ = child.wait();
    fs::remove_dir_all(&dir).ok();
}

/// Mixed-version replay: hand-build a WAL that interleaves v2
/// (with sentinel + CRC) and v3 (with type byte) records. Both
/// should land in the engine state on startup.
#[test]
fn interleaved_v2_and_v3_records_replay() {
    let dir = unique_tmpdir("mixed-replay");
    let db = dir.join("spg.db");
    let wal = dir.join("wal.log");

    let v2_record = |sql: &str| -> Vec<u8> {
        let len = sql.len() as u32;
        let crc = spg_crypto::crc32::crc32(sql.as_bytes());
        let mut out = Vec::new();
        out.extend_from_slice(&(len | WAL_V2_SENTINEL).to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(sql.as_bytes());
        out
    };
    let v3_record = |sql: &str| -> Vec<u8> {
        let len = sql.len() as u32;
        let mut crc_input = Vec::with_capacity(1 + sql.len());
        crc_input.push(WAL_V3_TYPE_AUTO_COMMIT_SQL);
        crc_input.extend_from_slice(sql.as_bytes());
        let crc = spg_crypto::crc32::crc32(&crc_input);
        let mut out = Vec::new();
        out.extend_from_slice(&(len | WAL_V2_SENTINEL | WAL_V3_FLAG).to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.push(WAL_V3_TYPE_AUTO_COMMIT_SQL);
        out.extend_from_slice(sql.as_bytes());
        out
    };

    let mut all = Vec::new();
    // v2-shaped CREATE + first INSERT.
    all.extend_from_slice(&v2_record("BEGIN"));
    all.extend_from_slice(&v2_record("CREATE TABLE mix (v INT)"));
    all.extend_from_slice(&v2_record("COMMIT"));
    all.extend_from_slice(&v2_record("BEGIN"));
    all.extend_from_slice(&v2_record("INSERT INTO mix VALUES (10)"));
    all.extend_from_slice(&v2_record("COMMIT"));
    // v3-shaped subsequent inserts.
    all.extend_from_slice(&v3_record("INSERT INTO mix VALUES (20)"));
    all.extend_from_slice(&v3_record("INSERT INTO mix VALUES (30)"));
    fs::write(&wal, &all).expect("write mixed WAL");

    let (raw_child, addr) = spawn_server_on_ephemeral_port(&db, &wal);
    let _child = ChildGuard(raw_child);
    let mut s = connect_to(&addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    assert_eq!(count_select_rows(&mut s, "SELECT * FROM mix"), 3);
    fs::remove_dir_all(&dir).ok();
}
