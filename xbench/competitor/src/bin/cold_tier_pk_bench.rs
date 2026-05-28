//! v5.1 → v5.2 L4 trigger validator. Drives a spg-server child
//! preloaded with a hand-baked sweep-schema segment and measures
//! PK lookup p50/p99 over N samples. Prints the timing alongside
//! the PG baseline (32717 µs @ 30M from v4.42 sweep) so the
//! caller can sign off `v5.1 → v5.2` once the comparison is in
//! the right direction.
//!
//! Workflow:
//!
//!   1. Resolve segment path (must already be baked via the
//!      `bake_segment` binary).
//!   2. Spawn spg-server with `SPG_PRELOAD_COLD_SEGMENT=
//!      sweep:sweep_id_idx:<path>`.
//!   3. Connect over spg-wire and CREATE TABLE + CREATE INDEX
//!      (those two statements together trip the lazy preload).
//!   4. Burn one `SELECT 1` to make sure the preload actually
//!      ran (and to amortise its cost out of the measurement).
//!   5. Run N PK lookups against random ids in `[1, rows]` and
//!      report p50 / p99 alongside the PG ceiling.
//!
//! Run:
//!
//!   cargo run --release -p spg-bench-competitor --bin bake_segment -- \
//!     --rows 30000000 --output /tmp/sweep_30m.spg
//!   cargo run --release -p spg-bench-competitor --bin cold_tier_pk_bench -- \
//!     --rows 30000000 --segment /tmp/sweep_30m.spg --samples 10000

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use spg_wire::{Frame, Op, build_query, encode, parse_data_row, parse_data_row_batch};

/// Per V5_DESIGN.md L4 trigger v5.1 → v5.2: PG @30M PK p99
/// recorded by v4.42 sweep. spg-server cold-tier PK p99 must
/// stay at or below this for the trigger to fire.
const PG_BASELINE_PK_P99_US_AT_30M: u128 = 32_717;

const DEFAULT_ROWS: u64 = 30_000_000;
const DEFAULT_SAMPLES: usize = 10_000;
const SERVER_ADDR: &str = "127.0.0.1:25561";
const SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const PRELOAD_WAIT: Duration = Duration::from_secs(180);

fn parse_args() -> (u64, PathBuf, usize) {
    let mut rows = DEFAULT_ROWS;
    let mut segment: Option<PathBuf> = None;
    let mut samples = DEFAULT_SAMPLES;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--rows" => {
                rows = args
                    .next()
                    .expect("--rows takes a value")
                    .parse()
                    .expect("--rows must be a number");
            }
            "--segment" => {
                segment = Some(PathBuf::from(args.next().expect("--segment takes a path")));
            }
            "--samples" => {
                samples = args
                    .next()
                    .expect("--samples takes a value")
                    .parse()
                    .expect("--samples must be a number");
            }
            "--help" | "-h" => {
                eprintln!(
                    "cold_tier_pk_bench: v5.1 cold-tier PK p99 validation\n\
                     Usage:\n  \
                       cargo run --release -p spg-bench-competitor --bin cold_tier_pk_bench -- \\\n  \
                         [--rows N=30000000] [--segment PATH] [--samples K=10000]\n"
                );
                std::process::exit(0);
            }
            other => panic!("unknown arg: {other}"),
        }
    }
    let segment = segment.unwrap_or_else(|| PathBuf::from(format!("/tmp/sweep_{rows}.spg")));
    if !segment.exists() {
        eprintln!(
            "cold_tier_pk_bench: segment {} not found — run `bake_segment --rows {rows} --output {}` first",
            segment.display(),
            segment.display()
        );
        std::process::exit(2);
    }
    (rows, segment, samples)
}

fn spawn_server_with_preload(
    addr: &str,
    segment_path: &PathBuf,
) -> Result<Child, Box<dyn std::error::Error>> {
    let build = Command::new("cargo")
        .args(["build", "--release", "-q", "-p", "spg-server"])
        .status()?;
    if !build.success() {
        return Err("cargo build spg-server failed".into());
    }
    let target_dir = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".into());
    let bin = format!("{target_dir}/release/spg-server");
    let tmp = std::env::temp_dir().join(format!("spg-cold-bench-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let db_path = tmp.join("a.db");
    let wal_path = tmp.join("a.wal");
    let preload_spec = format!("sweep:sweep_id_idx:{}", segment_path.display());
    let mut child = Command::new(&bin)
        .arg(addr)
        .arg(&db_path)
        .arg("-")
        .arg(&wal_path)
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD")
        .env_remove("SPG_PG_ADDR")
        .env("SPG_PRELOAD_COLD_SEGMENT", &preload_spec)
        .spawn()?;
    let stderr = child.stderr.take().expect("stderr piped");
    let mut reader = BufReader::new(stderr);
    let start = Instant::now();
    let mut line = String::new();
    while start.elapsed() < SERVER_STARTUP_TIMEOUT {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        eprintln!("[server] {}", line.trim_end());
        if line.contains("listening on") {
            // Drain remaining stderr asynchronously so the pipe
            // doesn't fill mid-bench, but echo lines so the caller
            // sees the cold preload log.
            std::thread::spawn(move || {
                let mut buf = String::new();
                let mut br = reader;
                while br.read_line(&mut buf).is_ok() {
                    if buf.is_empty() {
                        break;
                    }
                    eprintln!("[server] {}", buf.trim_end());
                    buf.clear();
                }
            });
            return Ok(child);
        }
    }
    let _ = child.kill();
    Err("spg-server didn't print 'listening on' in time".into())
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn read_frame(s: &mut TcpStream) -> std::io::Result<Frame> {
    let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
    s.read_exact(&mut header)?;
    let plen = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let op = Op::from_byte(header[4]).map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut payload = vec![0u8; plen];
    if plen > 0 {
        s.read_exact(&mut payload)?;
    }
    Ok(Frame { op, payload })
}

fn drain_to_cc(s: &mut TcpStream, sql: &str) -> std::io::Result<()> {
    loop {
        let f = read_frame(s)?;
        match f.op {
            Op::CommandComplete => return Ok(()),
            Op::ErrorResponse | Op::Error => {
                let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
                return Err(std::io::Error::other(format!("{sql:?}: {msg}")));
            }
            _ => {}
        }
    }
}

fn exec_ok(s: &mut TcpStream, sql: &str) -> std::io::Result<()> {
    let mut out = Vec::new();
    encode(&build_query(sql), &mut out).map_err(|e| std::io::Error::other(e.to_string()))?;
    s.write_all(&out)?;
    drain_to_cc(s, sql)
}

fn exec_count(s: &mut TcpStream, sql: &str) -> std::io::Result<usize> {
    let mut out = Vec::new();
    encode(&build_query(sql), &mut out).map_err(|e| std::io::Error::other(e.to_string()))?;
    s.write_all(&out)?;
    let mut total = 0usize;
    loop {
        let f = read_frame(s)?;
        match f.op {
            Op::CommandComplete => return Ok(total),
            Op::ErrorResponse | Op::Error => {
                let msg = spg_wire::parse_error_response(&f).unwrap_or("<undecodable>");
                return Err(std::io::Error::other(format!("{sql:?}: {msg}")));
            }
            Op::DataRow => {
                let _ = parse_data_row(&f);
                total += 1;
            }
            Op::DataRowBatch => {
                if let Ok(rows) = parse_data_row_batch(&f) {
                    total += rows.len();
                }
            }
            _ => {}
        }
    }
}

fn percentile(us: &mut [u128], p: f64) -> u128 {
    if us.is_empty() {
        return 0;
    }
    us.sort_unstable();
    let idx = ((us.len() as f64 - 1.0) * p).round() as usize;
    us[idx.min(us.len() - 1)]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (rows, segment_path, samples) = parse_args();
    eprintln!(
        "cold_tier_pk_bench: rows={rows} segment={} samples={samples}",
        segment_path.display()
    );

    let child = ChildGuard(spawn_server_with_preload(SERVER_ADDR, &segment_path)?);
    let _ = &child;

    let mut s = TcpStream::connect(SERVER_ADDR)?;
    s.set_nodelay(true).ok();
    s.set_read_timeout(Some(PRELOAD_WAIT))?;
    s.set_write_timeout(Some(Duration::from_secs(30)))?;

    eprintln!("cold_tier_pk_bench: CREATE TABLE + INDEX (will trigger lazy preload)");
    let t_setup = Instant::now();
    exec_ok(
        &mut s,
        "CREATE TABLE sweep (id INT NOT NULL, sec INT NOT NULL, name TEXT NOT NULL)",
    )?;
    exec_ok(&mut s, "CREATE INDEX sweep_id_idx ON sweep (id)")?;
    exec_ok(&mut s, "CREATE INDEX sweep_sec_idx ON sweep (sec)")?;
    eprintln!(
        "cold_tier_pk_bench: CREATE complete in {:.2}s",
        t_setup.elapsed().as_secs_f64()
    );

    // Burn one query so the preload's lazy load actually runs +
    // its wall time isn't billed against the first sample.
    let t_preload = Instant::now();
    let probe_id = 1_i64;
    let count = exec_count(
        &mut s,
        &format!("SELECT name FROM sweep WHERE id = {probe_id}"),
    )?;
    eprintln!(
        "cold_tier_pk_bench: first lookup id={probe_id} → {count} row(s) in {:.2}s \
         (includes one-time preload + register_cold_locators)",
        t_preload.elapsed().as_secs_f64()
    );
    if count == 0 {
        return Err(format!(
            "warm-up lookup id={probe_id} returned 0 rows — preload didn't happen?"
        )
        .into());
    }

    eprintln!("cold_tier_pk_bench: running {samples} PK lookup samples");
    let mut us_samples: Vec<u128> = Vec::with_capacity(samples);
    let mut hits = 0usize;
    for k in 0..samples {
        let target = ((k as u64).wrapping_mul(2_654_435_761) % rows + 1) as i64;
        let t = Instant::now();
        let n = exec_count(
            &mut s,
            &format!("SELECT name FROM sweep WHERE id = {target}"),
        )?;
        us_samples.push(t.elapsed().as_micros());
        if n > 0 {
            hits += 1;
        }
    }

    let p50 = percentile(&mut us_samples, 0.50);
    let p99 = percentile(&mut us_samples, 0.99);
    eprintln!(
        "cold_tier_pk_bench: {samples} samples, {hits} hits — p50={p50} µs, \
         p99={p99} µs (PG baseline p99 @ 30M = {PG_BASELINE_PK_P99_US_AT_30M} µs)"
    );
    if p99 <= PG_BASELINE_PK_P99_US_AT_30M {
        println!(
            "v5.1 → v5.2 trigger MET: spg-server cold-tier PK p99 = {p99} µs ≤ PG {PG_BASELINE_PK_P99_US_AT_30M} µs"
        );
    } else {
        println!(
            "v5.1 → v5.2 trigger NOT MET: spg-server cold-tier PK p99 = {p99} µs > PG {PG_BASELINE_PK_P99_US_AT_30M} µs"
        );
    }
    Ok(())
}
