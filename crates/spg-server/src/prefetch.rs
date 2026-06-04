//! v6.7.6 — boot-time prefetch worker pool for cold-tier segments.
//!
//! The pre-v6.7.6 boot path read every manifest-listed cold segment
//! file sequentially under the single boot thread. With cold tiers
//! sized to hundreds of segments × MiBs each, that fs::read loop is
//! the dominant contributor to "100M-rows cold start" latency.
//!
//! v6.7.6 parallelises the read step:
//!   - `SPG_PREFETCH_WORKERS` env (default `max(1, num_cpus()-2)`,
//!     capped at 16) sizes a `std::thread::scope`-driven worker pool.
//!   - Each worker is handed a slice of `(segment_id, path)` pairs.
//!     On Linux it issues `posix_fadvise(WILLNEED)` against the open
//!     fd before the read, so the kernel's read-ahead can overlap
//!     with the worker that's still finishing its previous segment.
//!   - The coordinator concatenates the per-worker outputs into a
//!     single `BTreeMap<u32, Vec<u8>>` for the caller to register
//!     with `Catalog::load_segment_bytes_at` serially.
//!
//! v6.7 carve-out: "prefetch triggered by `SegmentReader::scan` on
//! sequential access" is in the L2 spec but not active here — cold
//! segments currently live in their entirety in memory once loaded,
//! so there's no page-cache eviction to refresh between scans.
//! When v6.x cold-tier streaming lands, scan-triggered prefetch
//! reuses this same pool.
//!
//! The metric `spg_cold_prefetch_hits_total` increments once per
//! successfully prefetched segment.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::ServerState;

/// `SPG_PREFETCH_WORKERS` env override. Returns the user-set value
/// when it parses cleanly and is `> 0`; otherwise
/// `default_worker_count()`.
pub(crate) fn worker_count_from_env() -> usize {
    std::env::var("SPG_PREFETCH_WORKERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(default_worker_count)
}

/// `max(1, num_cpus()-2)`, capped at 16. Reserves 2 cores for the
/// dispatch + WAL threads so the boot prefetch doesn't starve
/// concurrent connection accept (the listener thread is alive
/// before manifest-load returns).
pub(crate) fn default_worker_count() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    cores.saturating_sub(2).max(1).min(16)
}

/// Read N segment files in parallel, returning per-segment results.
///
/// `state` is borrowed for the `cold_prefetch_hits` metric; pass
/// `None` to skip metric accounting (unit tests).
pub(crate) fn parallel_read_segments(
    paths: &[(u32, PathBuf)],
    workers: usize,
    state: Option<&Arc<ServerState>>,
) -> Vec<(u32, std::io::Result<Vec<u8>>)> {
    if paths.is_empty() {
        return Vec::new();
    }
    let workers = workers.max(1).min(paths.len());
    let chunk_size = paths.len().div_ceil(workers);
    if workers == 1 || paths.len() == 1 {
        return paths
            .iter()
            .map(|(id, path)| {
                let r = read_with_hint(path);
                if r.is_ok()
                    && let Some(s) = state
                {
                    s.metrics.cold_prefetch_hits.fetch_add(1, Ordering::Relaxed);
                }
                (*id, r)
            })
            .collect();
    }
    std::thread::scope(|scope| {
        let handles: Vec<_> = paths
            .chunks(chunk_size)
            .map(|chunk| {
                let state = state.cloned();
                scope.spawn(move || -> Vec<(u32, std::io::Result<Vec<u8>>)> {
                    chunk
                        .iter()
                        .map(|(id, path)| {
                            let r = read_with_hint(path);
                            if r.is_ok()
                                && let Some(ref s) = state
                            {
                                s.metrics.cold_prefetch_hits.fetch_add(1, Ordering::Relaxed);
                            }
                            (*id, r)
                        })
                        .collect()
                })
            })
            .collect();
        let mut out: Vec<(u32, std::io::Result<Vec<u8>>)> = Vec::with_capacity(paths.len());
        for h in handles {
            out.extend(h.join().expect("prefetch worker panicked"));
        }
        out
    })
}

/// Read a segment file, hinting `POSIX_FADV_WILLNEED` against the
/// open fd on Linux so the kernel's read-ahead overlaps with the
/// next worker's I/O. macOS / other platforms fall through to a
/// plain `std::fs::read`.
#[allow(unsafe_code)]
fn read_with_hint(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    #[cfg(target_os = "linux")]
    {
        use std::io::Read;
        use std::os::unix::io::AsRawFd;
        let mut f = std::fs::File::open(path)?;
        let fd = f.as_raw_fd();
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        // SAFETY: libc::posix_fadvise FFI. fd is a valid open file
        // descriptor for the lifetime of `f`; offset 0 + len bytes
        // is the entire file. `POSIX_FADV_WILLNEED` is advisory —
        // it never mutates the file, just seeds the page cache.
        // Errors are logged + ignored: the read still works without
        // the hint.
        let rc =
            unsafe { libc::posix_fadvise(fd, 0, len as libc::off_t, libc::POSIX_FADV_WILLNEED) };
        if rc != 0 {
            // EBADF/EINVAL/ESPIPE — fall through to plain read.
            eprintln!(
                "spg-server: posix_fadvise(WILLNEED) on {} failed with errno {rc}; \
                 continuing without prefetch hint",
                path.display()
            );
        }
        let mut bytes = Vec::with_capacity(len as usize);
        f.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // macOS / Windows / etc.: no posix_fadvise. fs::read is a
        // single read(2) call which still benefits from the OS's
        // default read-ahead.
        std::fs::read(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_segment(dir: &std::path::Path, id: u32, payload: &[u8]) -> PathBuf {
        let p = dir.join(format!("seg_{id}.spg"));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(payload).unwrap();
        p
    }

    #[test]
    fn parallel_read_segments_returns_every_input() {
        let dir = std::env::temp_dir().join(format!("spg-test-prefetch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths: Vec<(u32, PathBuf)> = (0..8)
            .map(|i| {
                let payload = vec![i as u8; 64];
                (i, tmp_segment(&dir, i, &payload))
            })
            .collect();
        let out = parallel_read_segments(&paths, 4, None);
        assert_eq!(out.len(), 8);
        for (id, r) in &out {
            let bytes = r.as_ref().expect("read ok");
            assert_eq!(bytes.len(), 64);
            assert!(bytes.iter().all(|b| *b == *id as u8));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parallel_read_segments_single_worker_path() {
        let dir =
            std::env::temp_dir().join(format!("spg-test-prefetch-single-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths: Vec<(u32, PathBuf)> = (0..3)
            .map(|i| {
                let payload = vec![i as u8; 32];
                (i, tmp_segment(&dir, i, &payload))
            })
            .collect();
        let out = parallel_read_segments(&paths, 1, None);
        assert_eq!(out.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parallel_read_segments_handles_empty() {
        let out = parallel_read_segments(&[], 4, None);
        assert!(out.is_empty());
    }
}
