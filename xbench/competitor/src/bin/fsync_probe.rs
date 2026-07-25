//! P0-2 probe — what does one durable commit actually cost on this filesystem?
//!
//! The 2026-07-25 audit measured SPGS at ~5.3 ms per durable commit against
//! PG18's ~0.91 ms on the same disk, with both doing exactly one fsync. This
//! isolates the fsync itself: SPG appends to a WAL file that keeps growing,
//! so every `sync_data` must also persist the new file length — and file
//! length is data-retrieval metadata, which is precisely what `fdatasync`
//! does NOT get to skip. PG pre-allocates 16 MB WAL segments and writes into
//! them, so its fsync flushes data blocks only.
//!
//! Run: cargo run --release -p spg-bench-competitor --bin fsync_probe [dir]

// The macOS control below calls `fsync(2)` directly — the whole point is to
// compare it against what `sync_data` does, and std offers no plain-fsync API.
#![allow(unsafe_code)]

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::time::Instant;

const RECORD: usize = 200; // ~ one INSERT's WAL record
const ITERS: usize = 200;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() -> std::io::Result<()> {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "/tmp".into());
    let buf = vec![b'x'; RECORD];

    // (a) EXTENDING file — what SPG's WAL append does today.
    let pa = format!("{dir}/fsync_probe_extend.bin");
    let _ = std::fs::remove_file(&pa);
    let mut fa = OpenOptions::new().create(true).append(true).open(&pa)?;
    let mut ext = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        let t = Instant::now();
        fa.write_all(&buf)?;
        fa.sync_data()?;
        ext.push(t.elapsed().as_secs_f64() * 1000.0);
    }

    // (b) PRE-ALLOCATED file, written in place — what PG's WAL segment does.
    let pb = format!("{dir}/fsync_probe_prealloc.bin");
    let _ = std::fs::remove_file(&pb);
    let mut fb = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&pb)?;
    fb.write_all(&vec![0u8; 16 * 1024 * 1024])?; // 16 MB, PG's segment size
    fb.sync_all()?;
    fb.seek(SeekFrom::Start(0))?;
    let mut pre = Vec::with_capacity(ITERS);
    for i in 0..ITERS {
        let t = Instant::now();
        fb.seek(SeekFrom::Start((i * RECORD) as u64))?;
        fb.write_all(&buf)?;
        fb.sync_data()?;
        pre.push(t.elapsed().as_secs_f64() * 1000.0);
    }

    // (c) macOS ONLY — plain fsync(2) vs what Rust's sync_data does.
    //
    // Rust maps `sync_data` to `F_FULLFSYNC` on Apple platforms: a real
    // device-cache flush. Plain `fsync(2)` on macOS returns once the data
    // reaches the drive's write cache and does NOT flush it. A Linux
    // container's `fdatasync` on a virtual disk is the weaker guarantee too.
    // So an SPG-on-macOS vs PG-in-a-Linux-container comparison is measuring
    // two different durability contracts, not two implementations.
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        let pd = format!("{dir}/fsync_probe_plain.bin");
        let _ = std::fs::remove_file(&pd);
        let mut fd = OpenOptions::new().create(true).append(true).open(&pd)?;
        let mut plain = Vec::with_capacity(ITERS);
        for _ in 0..ITERS {
            let t = Instant::now();
            fd.write_all(&buf)?;
            // SAFETY: fd is a live descriptor owned by `fd` for this call.
            let rc = unsafe { libc::fsync(fd.as_raw_fd()) };
            assert_eq!(rc, 0, "fsync failed");
            plain.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        println!(
            "# macOS control: plain fsync(2) median = {:.3} ms (sync_data above is F_FULLFSYNC)",
            median(plain)
        );
        let _ = std::fs::remove_file(pd);
    }

    // (d) write with NO fsync — the floor.
    let pc = format!("{dir}/fsync_probe_nosync.bin");
    let _ = std::fs::remove_file(&pc);
    let mut fc = OpenOptions::new().create(true).append(true).open(&pc)?;
    let mut nos = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        let t = Instant::now();
        fc.write_all(&buf)?;
        nos.push(t.elapsed().as_secs_f64() * 1000.0);
    }

    println!("# one-commit cost isolation — {ITERS} iters of a {RECORD}-byte record, dir={dir}");
    println!("| mode                              | median ms |");
    println!("|-----------------------------------|----------:|");
    println!("| append to GROWING file + sync_data | {:9.3} |", median(ext));
    println!("| write into PRE-ALLOCATED + sync_data | {:7.3} |", median(pre));
    println!("| append, no sync (floor)           | {:9.3} |", median(nos));
    for p in [pa, pb, pc] {
        let _ = std::fs::remove_file(p);
    }
    Ok(())
}
