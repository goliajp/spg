//! SPG CLI — v0.1 implements only `spg ping [addr]`.
//!
//! Future subcommands (`spg query`, `spg dump`, …) arrive alongside the SQL
//! layer in v0.2+.

use std::env;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process;
use std::time::Duration;

use spg_wire::{Frame, FrameError, Op, decode, encode};

const DEFAULT_ADDR: &str = "127.0.0.1:5544";
const READ_TIMEOUT: Duration = Duration::from_secs(3);

fn main() {
    let mut args = env::args().skip(1);
    let cmd = args.next();
    let addr = args.next().unwrap_or_else(|| DEFAULT_ADDR.to_string());

    match cmd.as_deref() {
        Some("ping") => match ping(&addr) {
            Ok(()) => println!("PONG"),
            Err(e) => {
                eprintln!("spg: ping failed: {e}");
                process::exit(1);
            }
        },
        Some(other) => {
            eprintln!("spg: unknown command: {other}");
            usage();
            process::exit(2);
        }
        None => {
            usage();
            process::exit(2);
        }
    }
}

fn usage() {
    eprintln!("usage: spg <ping> [addr]");
    eprintln!("       addr defaults to {DEFAULT_ADDR}");
}

fn ping(addr: &str) -> Result<(), String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|e| format!("set_read_timeout: {e}"))?;

    let mut out = Vec::new();
    encode(&Frame::ping(), &mut out).map_err(|e| format!("encode: {e}"))?;
    stream.write_all(&out).map_err(|e| format!("write: {e}"))?;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 64];
    loop {
        match decode(&buf) {
            Ok((frame, _)) => {
                return match frame.op {
                    Op::Pong => Ok(()),
                    Op::Error => Err(format!(
                        "server error: {}",
                        String::from_utf8_lossy(&frame.payload)
                    )),
                    Op::Ping => Err("server replied with PING (expected PONG)".into()),
                };
            }
            Err(FrameError::ShortHeader | FrameError::ShortPayload) => {
                let n = stream.read(&mut chunk).map_err(|e| format!("read: {e}"))?;
                if n == 0 {
                    return Err("connection closed before pong".into());
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(e) => return Err(format!("decode: {e}")),
        }
    }
}
