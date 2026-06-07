//! v7.13.0 — `pg_isready` shim (mailrs round-5 C5).
//!
//! Drop-in for PG's `pg_isready` from the official postgres image.
//! Docker-compose / k8s healthchecks call it without arguments and
//! expect exit code 0 = ready, 2 = not ready. We open a TCP probe on
//! the configured PG-wire address and return one of those codes.
//!
//! Flags (PG-compatible subset):
//!   -h <host>   host (default: 127.0.0.1)
//!   -p <port>   port (default: 5432 — same as the official PG image,
//!               matches SPG's v7.13 PG-wire default port)
//!   -t <secs>   connect timeout (default: 3)
//!   -q          quiet (suppress stdout)
//!
//! The body of the connect is a single TCP `connect()` with the
//! configured timeout. We don't speak the PG protocol — that would
//! require a startup handshake; for `pg_isready` semantics, "the
//! listener accepts a TCP connection" is equivalent to "ready" since
//! the spg-server boot path opens the listener only after WAL replay
//! finishes.

use std::env;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::process;
use std::time::Duration;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut host: Option<String> = None;
    let mut port: Option<u16> = None;
    let mut timeout_secs: u64 = 3;
    let mut quiet = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    host = Some(v.clone());
                }
            }
            "-p" => {
                i += 1;
                if let Some(v) = args.get(i)
                    && let Ok(p) = v.parse::<u16>()
                {
                    port = Some(p);
                }
            }
            "-t" => {
                i += 1;
                if let Some(v) = args.get(i)
                    && let Ok(t) = v.parse::<u64>()
                {
                    timeout_secs = t;
                }
            }
            "-q" | "--quiet" => quiet = true,
            "-V" | "--version" => {
                println!("pg_isready (SPG) {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "-?" | "--help" => {
                println!(
                    "Usage: pg_isready [-h host] [-p port] [-t timeout] [-q]\n\
                     SPG's pg_isready probes the PG-wire listener with a TCP \
                     connect; exits 0 if reachable, 2 otherwise."
                );
                return;
            }
            _ => {
                // Unknown flag — match PG behaviour and continue.
                i += 1;
                continue;
            }
        }
        i += 1;
    }
    let host = host
        .or_else(|| env::var("PGHOST").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "127.0.0.1".into());
    let port = port
        .or_else(|| env::var("PGPORT").ok().and_then(|s| s.parse::<u16>().ok()))
        .unwrap_or(5432);
    let target = format!("{host}:{port}");
    let addrs: Vec<SocketAddr> = match target.to_socket_addrs() {
        Ok(it) => it.collect(),
        Err(e) => {
            if !quiet {
                eprintln!("pg_isready: resolve {target:?}: {e}");
            }
            process::exit(2);
        }
    };
    let deadline = Duration::from_secs(timeout_secs);
    for addr in &addrs {
        if TcpStream::connect_timeout(addr, deadline).is_ok() {
            if !quiet {
                println!("{target} - accepting connections");
            }
            return;
        }
    }
    if !quiet {
        println!("{target} - no response");
    }
    process::exit(2);
}
