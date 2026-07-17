//! v7.39 (read01 round 131) — a `RETURNING` result carries the data-modifying
//! statement's own CommandComplete tag over the wire (`INSERT 0 n` / `UPDATE n`
//! / `DELETE n` / `MERGE n`), matching PG 18.4. Before this, every RETURNING
//! result tagged `SELECT n` (shared across all four DML forms). Verified over
//! the real pgwire protocol — the tag is a wire-only field.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

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

/// The CommandComplete (`C`) tag string, NUL stripped.
fn command_tag(msgs: &[PgMessage]) -> String {
    let c = msgs.iter().find(|m| m.ty == b'C').expect("CommandComplete");
    let end = c.body.iter().position(|&b| b == 0).unwrap_or(c.body.len());
    String::from_utf8_lossy(&c.body[..end]).into_owned()
}

fn open(addr: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    send_startup(&mut s, "admin");
    let _ = read_until_ready(&mut s);
    s
}

fn tag_of(s: &mut TcpStream, sql: &str) -> String {
    send_query(s, sql);
    command_tag(&read_until_ready(s))
}

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("spg-e2e-rettag-{label}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn local_spawn(db: &std::path::Path) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(db)
        .with_pgwire()
        .spawn()
}

#[test]
fn returning_tags_match_pg() {
    let dir = unique_tmpdir("main");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    tag_of(&mut s, "CREATE TABLE wt(id int primary key, v int)");
    tag_of(&mut s, "CREATE TABLE ws(id int, v int)");
    tag_of(&mut s, "INSERT INTO wt VALUES(1,10),(2,20)");
    tag_of(&mut s, "INSERT INTO ws VALUES(1,100),(3,300)");

    // INSERT RETURNING → "INSERT 0 2" (not "SELECT 2").
    assert_eq!(
        tag_of(&mut s, "INSERT INTO wt VALUES(5,50),(6,60) RETURNING id"),
        "INSERT 0 2"
    );
    // UPDATE RETURNING → "UPDATE 2".
    assert_eq!(
        tag_of(&mut s, "UPDATE wt SET v=v+1 WHERE id<=2 RETURNING id"),
        "UPDATE 2"
    );
    // DELETE RETURNING → "DELETE 1".
    assert_eq!(
        tag_of(&mut s, "DELETE FROM wt WHERE id=6 RETURNING id"),
        "DELETE 1"
    );
    // MERGE RETURNING → "MERGE 2".
    assert_eq!(
        tag_of(
            &mut s,
            "MERGE INTO wt t USING ws s ON t.id=s.id \
             WHEN MATCHED THEN UPDATE SET v=s.v \
             WHEN NOT MATCHED THEN INSERT VALUES(s.id,s.v) \
             RETURNING merge_action()"
        ),
        "MERGE 2"
    );
    // Data-modifying CTE tags by its top-level statement.
    assert_eq!(
        tag_of(
            &mut s,
            "WITH c AS (SELECT 7 AS id) INSERT INTO wt SELECT id, 70 FROM c RETURNING id"
        ),
        "INSERT 0 1"
    );
}

/// Run one SQL via the extended protocol (Parse/Bind/Execute/Sync, no params)
/// and return its CommandComplete tag — the path sqlx / tokio-postgres use.
fn ext_tag(s: &mut TcpStream, sql: &str) -> String {
    // Parse (unnamed statement).
    let mut p = Vec::new();
    p.push(0); // stmt name ""
    p.extend_from_slice(sql.as_bytes());
    p.push(0);
    p.extend_from_slice(&0i16.to_be_bytes()); // 0 param types
    send_msg(s, b'P', &p);
    // Bind (unnamed portal ← unnamed statement).
    let mut b = Vec::new();
    b.push(0); // portal ""
    b.push(0); // stmt ""
    b.extend_from_slice(&0i16.to_be_bytes()); // 0 format codes
    b.extend_from_slice(&0i16.to_be_bytes()); // 0 params
    b.extend_from_slice(&0i16.to_be_bytes()); // 0 result formats
    send_msg(s, b'B', &b);
    // Execute (unlimited rows).
    let mut e = Vec::new();
    e.push(0); // portal ""
    e.extend_from_slice(&0i32.to_be_bytes());
    send_msg(s, b'E', &e);
    send_msg(s, b'S', &[]); // Sync
    command_tag(&read_until_ready(s))
}

#[test]
fn extended_protocol_returning_tags() {
    let dir = unique_tmpdir("ext");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    tag_of(&mut s, "CREATE TABLE t(id int primary key, v int)");
    tag_of(&mut s, "INSERT INTO t VALUES(1,10),(2,20)");

    // Extended-protocol RETURNING tags (sqlx path) match PG too.
    assert_eq!(
        ext_tag(&mut s, "INSERT INTO t VALUES(9,90) RETURNING id"),
        "INSERT 0 1"
    );
    assert_eq!(
        ext_tag(&mut s, "UPDATE t SET v=v+1 WHERE id<=2 RETURNING id"),
        "UPDATE 2"
    );
    assert_eq!(
        ext_tag(&mut s, "DELETE FROM t WHERE id=9 RETURNING id"),
        "DELETE 1"
    );
    // Plain SELECT via extended protocol still tags "SELECT n".
    assert_eq!(ext_tag(&mut s, "SELECT id FROM t"), "SELECT 2");
}

#[test]
fn plain_select_and_nonreturning_dml_unchanged() {
    let dir = unique_tmpdir("regress");
    let db = dir.join("spg.db");
    let (raw, addrs) = local_spawn(&db);
    let _child = common::ChildGuard(raw);
    let mut s = open(addrs.pgwire.as_ref().unwrap());

    tag_of(&mut s, "CREATE TABLE t(id int)");
    tag_of(&mut s, "INSERT INTO t VALUES(1),(2),(3)");

    // Plain SELECT still tags "SELECT n".
    assert_eq!(tag_of(&mut s, "SELECT id FROM t"), "SELECT 3");
    assert_eq!(tag_of(&mut s, "SELECT 1"), "SELECT 1");
    // INSERT without RETURNING keeps "INSERT 0 n" (CommandOk path).
    assert_eq!(tag_of(&mut s, "INSERT INTO t VALUES(4)"), "INSERT 0 1");
    // UPDATE without RETURNING keeps "UPDATE n".
    assert_eq!(tag_of(&mut s, "UPDATE t SET id=id WHERE id>2"), "UPDATE 2");
}
