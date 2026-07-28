//! v7.39 (round 602) — what one `sync_data` on an append-only WAL actually
//! costs, measured the way the server does it.
//!
//! Round 601 measured 0.041 ms with a Python loop and concluded the commit
//! path's ~4.5 ms could not be the sync. A stack-attributed profile of the
//! server then put 77% of the connection thread's kernel time under
//! `wal::client_fsync`, which is one `File::sync_data`. Both cannot be
//! right, so this measures it from Rust, on the same filesystem, with the
//! server's own access pattern: `OpenOptions::append` on a file that grows.
use std::fs::OpenOptions;
use std::io::Write;
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "/tmp/fsync_probe.log".into());
    let n: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let _ = std::fs::remove_file(&path);
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("open");
    // Warm: one write + sync so the file exists and metadata is settled.
    f.write_all(&[0u8; 128]).unwrap();
    f.sync_data().unwrap();

    let t = Instant::now();
    for _ in 0..n {
        f.write_all(&[7u8; 128]).unwrap();
        f.sync_data().unwrap();
    }
    let each = t.elapsed().as_secs_f64() * 1000.0 / n as f64;

    // And the write alone, to separate the two halves.
    let t2 = Instant::now();
    for _ in 0..n {
        f.write_all(&[7u8; 128]).unwrap();
    }
    let w = t2.elapsed().as_secs_f64() * 1000.0 / n as f64;

    println!("{path}: write+sync_data {each:.3} ms/call, write alone {w:.4} ms/call ({n} calls)");
    let _ = std::fs::remove_file(&path);
}
