//! read01 round 179 — kill+restart differential matrix over DML shapes
//! and wire protocols.
//!
//! r178's restart pin caught a silent WAL loss (RETURNING DML never
//! persisted) that had been latent since v7.33 because no test ever
//! compared "rows the client was acked" against "rows after kill -9 +
//! WAL replay" across statement shapes. This harness does exactly
//! that, systematically: every DML shape (bare / RETURNING /
//! multi-row RETURNING / explicit tx / tx+RETURNING / UPDATE+DELETE
//! RETURNING / writable-CTE WITH / MERGE / MERGE RETURNING) executed
//! over BOTH the native wire and pgwire (simple query), plus the
//! pgwire extended protocol for the r178-fixed prepared path — then
//! one kill -9, one restart, and a per-shape row-count audit.
//!
//! A shape that passes pre-kill but fails post-restart is a
//! durability hole; a shape that fails pre-kill is an execution bug.

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use spg_wire::{
    FRAME_HEADER_LEN, Frame, Op, build_query, encode, parse_command_complete, parse_data_row,
    parse_data_row_batch,
};

const READ_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------- shapes

struct Shape {
    name: &'static str,
    /// Statements run before the writes (seeding); `{t}` = table name.
    seed: &'static [&'static str],
    /// The writes under audit.
    writes: &'static [&'static str],
    /// Row-returning verify query; its row count is the audit value.
    verify: &'static str,
    expect: usize,
}

const SHAPES: &[Shape] = &[
    Shape {
        name: "bare_insert",
        seed: &[],
        writes: &[
            "INSERT INTO {t} VALUES (1, 0)",
            "INSERT INTO {t} VALUES (2, 0)",
            "INSERT INTO {t} VALUES (3, 0)",
        ],
        verify: "SELECT id FROM {t}",
        expect: 3,
    },
    Shape {
        name: "insert_returning",
        seed: &[],
        writes: &[
            "INSERT INTO {t} VALUES (1, 0) RETURNING id",
            "INSERT INTO {t} VALUES (2, 0) RETURNING id",
        ],
        verify: "SELECT id FROM {t}",
        expect: 2,
    },
    Shape {
        name: "multirow_ins_ret",
        seed: &[],
        writes: &["INSERT INTO {t} VALUES (1, 0), (2, 0), (3, 0) RETURNING id"],
        verify: "SELECT id FROM {t}",
        expect: 3,
    },
    Shape {
        name: "tx_commit",
        seed: &[],
        writes: &[
            "BEGIN",
            "INSERT INTO {t} VALUES (1, 0)",
            "INSERT INTO {t} VALUES (2, 0)",
            "COMMIT",
        ],
        verify: "SELECT id FROM {t}",
        expect: 2,
    },
    Shape {
        name: "tx_returning",
        seed: &[],
        writes: &[
            "BEGIN",
            "INSERT INTO {t} VALUES (1, 0) RETURNING id",
            "INSERT INTO {t} VALUES (2, 0) RETURNING id",
            "COMMIT",
        ],
        verify: "SELECT id FROM {t}",
        expect: 2,
    },
    Shape {
        name: "update_returning",
        seed: &[
            "INSERT INTO {t} VALUES (1, 0)",
            "INSERT INTO {t} VALUES (2, 0)",
            "INSERT INTO {t} VALUES (3, 0)",
        ],
        writes: &["UPDATE {t} SET v = 9 WHERE id <= 2 RETURNING id"],
        verify: "SELECT id FROM {t} WHERE v = 9",
        expect: 2,
    },
    Shape {
        name: "delete_returning",
        seed: &[
            "INSERT INTO {t} VALUES (1, 0)",
            "INSERT INTO {t} VALUES (2, 0)",
            "INSERT INTO {t} VALUES (3, 0)",
        ],
        writes: &["DELETE FROM {t} WHERE id = 1 RETURNING id"],
        verify: "SELECT id FROM {t}",
        expect: 2,
    },
    Shape {
        name: "with_insert",
        seed: &[],
        writes: &["WITH s AS (SELECT 7 AS x) INSERT INTO {t} SELECT x, 0 FROM s"],
        verify: "SELECT id FROM {t}",
        expect: 1,
    },
    Shape {
        name: "with_ins_ret",
        seed: &[],
        writes: &["WITH s AS (SELECT 7 AS x) INSERT INTO {t} SELECT x, 0 FROM s RETURNING id"],
        verify: "SELECT id FROM {t}",
        expect: 1,
    },
    Shape {
        name: "merge",
        seed: &[
            "INSERT INTO {t} VALUES (1, 0)",
            "CREATE TABLE {t}_src (id BIGINT, v BIGINT)",
            "INSERT INTO {t}_src VALUES (1, 5)",
            "INSERT INTO {t}_src VALUES (2, 6)",
        ],
        writes: &["MERGE INTO {t} m USING {t}_src s ON m.id = s.id \
             WHEN MATCHED THEN UPDATE SET v = s.v \
             WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.v)"],
        verify: "SELECT id FROM {t}",
        expect: 2,
    },
    Shape {
        name: "merge_returning",
        seed: &[
            "INSERT INTO {t} VALUES (1, 0)",
            "CREATE TABLE {t}_src (id BIGINT, v BIGINT)",
            "INSERT INTO {t}_src VALUES (1, 5)",
            "INSERT INTO {t}_src VALUES (2, 6)",
        ],
        writes: &["MERGE INTO {t} m USING {t}_src s ON m.id = s.id \
             WHEN MATCHED THEN UPDATE SET v = s.v \
             WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.v) \
             RETURNING *"],
        verify: "SELECT id FROM {t}",
        expect: 2,
    },
];

fn unique_tmpdir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = crate::common::tmp_base().join(format!("spg-dml-matrix-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn_server(dir: &std::path::Path) -> (std::process::Child, common::ServerAddrs) {
    common::ServerBuilder::new()
        .arg_path(&dir.join("d.spgdb"))
        .arg("-")
        .arg_path(&dir.join("d.wal"))
        .with_pgwire()
        .spawn()
}

// ------------------------------------------------------- native client

fn nat_send(stream: &mut TcpStream, sql: &str) {
    let mut out = Vec::new();
    encode(&build_query(sql), &mut out).unwrap();
    stream.write_all(&out).unwrap();
}

fn nat_frame(stream: &mut TcpStream) -> Frame {
    let mut header = [0u8; FRAME_HEADER_LEN];
    stream.read_exact(&mut header).expect("read header");
    let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).expect("known op");
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        stream.read_exact(&mut payload).expect("read payload");
    }
    Frame { op, payload }
}

/// Run one native statement, draining until CommandComplete; RETURNING
/// row frames are drained too. Panics (with context) on error frames.
fn nat_exec(stream: &mut TcpStream, sql: &str, ctx: &str) {
    nat_send(stream, sql);
    loop {
        let f = nat_frame(stream);
        match f.op {
            Op::CommandComplete => {
                parse_command_complete(&f).unwrap();
                return;
            }
            Op::RowDescription | Op::DataRow | Op::DataRowBatch => {}
            other => {
                let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
                panic!("[{ctx}] native exec failed for {sql:?}: {other:?} {msg}");
            }
        }
    }
}

fn nat_count(stream: &mut TcpStream, sql: &str) -> usize {
    nat_send(stream, sql);
    assert_eq!(nat_frame(stream).op, Op::RowDescription);
    let mut n = 0;
    loop {
        let f = nat_frame(stream);
        match f.op {
            Op::DataRow => n += parse_data_row(&f).map(|_| 1).unwrap_or(1),
            Op::DataRowBatch => n += parse_data_row_batch(&f).unwrap().len(),
            Op::CommandComplete => return n,
            other => panic!("unexpected {other:?}"),
        }
    }
}

// ------------------------------------------------------- pgwire client

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
    body.extend_from_slice(&196608u32.to_be_bytes());
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

fn pg_send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total = (body.len() + 4) as u32;
    let mut out = Vec::new();
    out.push(b'Q');
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&body);
    s.write_all(&out).unwrap();
}

/// Run one simple query; returns data-row count; panics on ErrorResponse.
fn pg_exec(s: &mut TcpStream, sql: &str, ctx: &str) -> usize {
    pg_send_query(s, sql);
    let mut rows = 0;
    let mut err: Option<String> = None;
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'D' => rows += 1,
            b'E' => err = Some(String::from_utf8_lossy(&body).into_owned()),
            b'Z' => {
                if let Some(e) = err {
                    panic!("[{ctx}] pgwire exec failed for {sql:?}: {e}");
                }
                return rows;
            }
            _ => {}
        }
    }
}

/// Extended-protocol execution of a parameterless statement: Parse +
/// Bind + Execute + Sync on the unnamed statement/portal.
fn pg_ext_exec(s: &mut TcpStream, sql: &str, ctx: &str) -> usize {
    let mut out = Vec::new();
    // Parse: name "" + sql + 0 param types
    let mut body = Vec::new();
    body.push(0); // empty statement name
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    body.extend_from_slice(&0u16.to_be_bytes());
    out.push(b'P');
    out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&body);
    // Bind: portal "" + stmt "" + 0 fmt + 0 params + 0 result fmt
    let mut body = Vec::new();
    body.push(0);
    body.push(0);
    body.extend_from_slice(&0u16.to_be_bytes());
    body.extend_from_slice(&0u16.to_be_bytes());
    body.extend_from_slice(&0u16.to_be_bytes());
    out.push(b'B');
    out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&body);
    // Execute: portal "" + max rows 0
    let mut body = Vec::new();
    body.push(0);
    body.extend_from_slice(&0u32.to_be_bytes());
    out.push(b'E');
    out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&body);
    // Sync
    out.push(b'S');
    out.extend_from_slice(&4u32.to_be_bytes());
    s.write_all(&out).unwrap();
    let mut rows = 0;
    let mut err: Option<String> = None;
    loop {
        let (ty, body) = pg_msg(s);
        match ty {
            b'D' => rows += 1,
            b'E' => err = Some(String::from_utf8_lossy(&body).into_owned()),
            b'Z' => {
                if let Some(e) = err {
                    panic!("[{ctx}] pgwire extended exec failed for {sql:?}: {e}");
                }
                return rows;
            }
            _ => {}
        }
    }
}

// --------------------------------------------------------------- test

fn subst(template: &str, table: &str) -> String {
    template.replace("{t}", table)
}

#[test]
fn dml_kill_restart_matrix() {
    let dir = unique_tmpdir();
    // (table, verify_sql, expect) audit list, filled while writing.
    let mut audits: Vec<(String, String, usize)> = Vec::new();
    {
        let (raw, addrs) = spawn_server(&dir);
        let mut guard = common::ChildGuard(raw);
        let mut nat = common::connect_to(&addrs.native);
        nat.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        let pg_addr = addrs.pgwire.clone().unwrap();
        let mut pg = pg_connect(&pg_addr);

        for shape in SHAPES {
            // Native-wire copy of the shape.
            let t = format!("n_{}", shape.name);
            let ctx = format!("native/{}", shape.name);
            nat_exec(
                &mut nat,
                &format!("CREATE TABLE {t} (id BIGINT, v BIGINT)"),
                &ctx,
            );
            for s in shape.seed {
                nat_exec(&mut nat, &subst(s, &t), &ctx);
            }
            for s in shape.writes {
                nat_exec(&mut nat, &subst(s, &t), &ctx);
            }
            audits.push((ctx, subst(shape.verify, &t), shape.expect));

            // pgwire simple-query copy.
            let t = format!("p_{}", shape.name);
            let ctx = format!("pgwire/{}", shape.name);
            pg_exec(
                &mut pg,
                &format!("CREATE TABLE {t} (id BIGINT, v BIGINT)"),
                &ctx,
            );
            for s in shape.seed {
                pg_exec(&mut pg, &subst(s, &t), &ctx);
            }
            for s in shape.writes {
                pg_exec(&mut pg, &subst(s, &t), &ctx);
            }
            audits.push((ctx, subst(shape.verify, &t), shape.expect));
        }

        // pgwire extended protocol — the r178-fixed prepared path.
        for (name, write, expect) in [
            ("ext_bare", "INSERT INTO {t} VALUES (1, 0)", 1),
            (
                "ext_returning",
                "INSERT INTO {t} VALUES (1, 0) RETURNING id",
                1,
            ),
        ] {
            let t = format!("e_{name}");
            let ctx = format!("pgwire-ext/{name}");
            pg_exec(
                &mut pg,
                &format!("CREATE TABLE {t} (id BIGINT, v BIGINT)"),
                &ctx,
            );
            pg_ext_exec(&mut pg, &subst(write, &t), &ctx);
            audits.push((ctx, format!("SELECT id FROM {t}"), expect));
        }

        // Pre-kill audit: what the engine believes right now.
        for (ctx, verify, expect) in &audits {
            let got = nat_count(&mut nat, verify);
            assert_eq!(got, *expect, "[{ctx}] PRE-KILL mismatch for {verify:?}");
        }
        let _ = guard.0.kill();
        let _ = guard.0.wait();
    }

    // Post-restart audit: what survived the WAL replay.
    let (raw, addrs) = spawn_server(&dir);
    let _guard = common::ChildGuard(raw);
    let mut nat = common::connect_to(&addrs.native);
    nat.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let mut failures = Vec::new();
    for (ctx, verify, expect) in &audits {
        let got = nat_count(&mut nat, verify);
        if got != *expect {
            failures.push(format!("[{ctx}] {verify:?}: expected {expect}, got {got}"));
        }
    }
    assert!(
        failures.is_empty(),
        "durability holes after kill+restart:\n{}",
        failures.join("\n")
    );
}
