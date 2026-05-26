//! v4.16 leak audit — exercises every v4.x code path in a tight
//! loop and watches RSS drift. Targets the new memory shapes:
//! SCRAM secret churn, CTE temp-engine clones, window-function
//! partition Vecs, subquery AST clones, JSON parser allocations,
//! observability HTTP listener threads.
//!
//! Run: `cargo run --release -p spg-bench-competitor --bin soak_v4 [--minutes N]`
//!
//! Default 5 minutes. Reports start/end RSS and per-100-cycle RSS
//! samples. A run that ends within ±2% of the start RSS is
//! considered leak-free; the previous v3.4.2 baseline established
//! ±0.2% as achievable for the readonly path.

#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::format_push_string,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unreadable_literal,
    clippy::useless_conversion
)]

use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SPG_NATIVE: &str = "127.0.0.1:25551";
const SPG_HTTP: &str = "127.0.0.1:25552";
const DEFAULT_MINUTES: u64 = 5;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let minutes = parse_minutes();
    let mut child = spawn_spg_server()?;
    let res = run(minutes);
    let _ = child.kill();
    let _ = child.wait();
    res
}

fn parse_minutes() -> u64 {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--minutes"
            && let Some(v) = args.next().and_then(|s| s.parse::<u64>().ok())
        {
            return v;
        }
    }
    DEFAULT_MINUTES
}

fn run(minutes: u64) -> Result<(), Box<dyn std::error::Error>> {
    // Seed: catalog, JSON docs, vector index, users, the lot.
    let stream = TcpStream::connect(SPG_NATIVE)?;
    stream.set_nodelay(true)?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::with_capacity(64 * 1024, stream);
    round_trip(
        &mut writer,
        &mut reader,
        "CREATE TABLE docs (id INT NOT NULL, body JSON NOT NULL)",
    )?;
    round_trip(
        &mut writer,
        &mut reader,
        "CREATE INDEX docs_id_idx ON docs (id)",
    )?;
    round_trip(
        &mut writer,
        &mut reader,
        "CREATE TABLE sales (region TEXT NOT NULL, amt INT NOT NULL)",
    )?;
    round_trip(
        &mut writer,
        &mut reader,
        "CREATE TABLE allowed (val INT NOT NULL)",
    )?;
    for i in 1..=200 {
        round_trip(
            &mut writer,
            &mut reader,
            &format!(
                "INSERT INTO docs VALUES ({i}, '{{\"k\":\"v{i}\",\"nested\":{{\"x\":{i}}}}}')"
            ),
        )?;
    }
    for i in 1..=200 {
        let region = if i % 2 == 0 { "east" } else { "west" };
        round_trip(
            &mut writer,
            &mut reader,
            &format!("INSERT INTO sales VALUES ('{region}', {i})"),
        )?;
    }
    for i in (10..=200).step_by(20) {
        round_trip(
            &mut writer,
            &mut reader,
            &format!("INSERT INTO allowed VALUES ({i})"),
        )?;
    }
    eprintln!(
        "soak_v4: seeded — running for {minutes} min, sampling RSS every 30 s and per 100 cycles"
    );

    let pid = server_pid();
    let start_rss = rss_kib(pid);
    let start = Instant::now();
    let deadline = start + Duration::from_secs(minutes * 60);
    let mut last_sample = start;
    let mut samples: Vec<(u64, u64)> = vec![(0, start_rss)];
    let mut cycles: u64 = 0;
    while Instant::now() < deadline {
        cycle(&mut writer, &mut reader, cycles)?;
        cycles += 1;
        if cycles.is_multiple_of(100) {
            // HTTP poll — exercise the observability listener
            let _ = poll_metrics();
        }
        if last_sample.elapsed() >= Duration::from_secs(30) {
            let rss = rss_kib(pid);
            let elapsed_s = start.elapsed().as_secs();
            samples.push((elapsed_s, rss));
            eprintln!(
                "  t={elapsed_s:>4}s  cycles={cycles:>7}  rss={} KiB  drift={:+.1}%",
                rss,
                drift_pct(start_rss, rss)
            );
            last_sample = Instant::now();
        }
    }
    let final_rss = rss_kib(pid);
    samples.push((start.elapsed().as_secs(), final_rss));
    print_report(start_rss, final_rss, cycles, &samples);
    Ok(())
}

fn cycle<W: Write, R: Read>(w: &mut W, r: &mut BufReader<R>, cycles: u64) -> Result<(), String> {
    // 1. JSON path access (exercises engine/json.rs allocator)
    let id = (cycles % 200) + 1;
    drain_query(
        w,
        r,
        &format!("SELECT body ->> 'k' FROM docs WHERE id = {id}"),
    )?;
    // 2. Subquery (exercises subquery resolver clone path)
    drain_query(
        w,
        r,
        "SELECT count(*) FROM sales WHERE amt IN (SELECT val FROM allowed)",
    )?;
    // 3. CTE (exercises temp-engine catalog clone path)
    drain_query(
        w,
        r,
        "WITH big AS (SELECT amt FROM sales WHERE amt > 50) SELECT count(*) FROM big",
    )?;
    // 4. Window function (exercises partition Vec alloc path)
    drain_query(
        w,
        r,
        "SELECT region, amt, ROW_NUMBER() OVER (PARTITION BY region ORDER BY amt) FROM sales",
    )?;
    // 5. SCRAM secret churn (CREATE USER then DROP USER per cycle).
    let user = format!("u{cycles}");
    round_trip(
        w,
        r,
        &format!("CREATE USER '{user}' WITH PASSWORD 'p' ROLE 'readonly'"),
    )?;
    round_trip(w, r, &format!("DROP USER '{user}'"))?;
    Ok(())
}

fn drain_query<W: Write, R: Read>(
    w: &mut W,
    r: &mut BufReader<R>,
    sql: &str,
) -> Result<(), String> {
    use spg_wire::{Op, build_query, encode};
    let mut out = Vec::with_capacity(sql.len() + 16);
    encode(&build_query(sql), &mut out).map_err(|e| format!("encode: {e}"))?;
    w.write_all(&out).map_err(|e| format!("write: {e}"))?;
    // Drain frames until CommandComplete or ErrorResponse.
    loop {
        let mut header = [0u8; spg_wire::FRAME_HEADER_LEN];
        r.read_exact(&mut header)
            .map_err(|e| format!("header: {e}"))?;
        let plen = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let op = Op::from_byte(header[4]).map_err(|e| format!("op: {e}"))?;
        let mut payload = vec![0u8; plen];
        if plen > 0 {
            r.read_exact(&mut payload)
                .map_err(|e| format!("payload: {e}"))?;
        }
        match op {
            Op::CommandComplete => return Ok(()),
            Op::ErrorResponse | Op::Error => {
                let msg = spg_wire::parse_error_response(&spg_wire::Frame { op, payload })
                    .map_or_else(|_| "<undecodable>".into(), str::to_owned);
                return Err(format!("query {sql:?} failed: {msg}"));
            }
            _ => {}
        }
    }
}

fn round_trip<W: Write, R: Read>(w: &mut W, r: &mut BufReader<R>, sql: &str) -> Result<(), String> {
    drain_query(w, r, sql)
}

fn poll_metrics() -> Result<(), String> {
    let mut s = TcpStream::connect(SPG_HTTP).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok();
    s.write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    Ok(())
}

fn server_pid() -> u32 {
    // We don't have Child::id() handy here (server was spawned in main).
    // Look up `spg-server` by name; assumes the soak is the only one.
    let out = Command::new("pgrep")
        .args(["-n", "spg-server"])
        .output()
        .expect("pgrep");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("pid parse")
}

fn rss_kib(pid: u32) -> u64 {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

fn drift_pct(start: u64, end: u64) -> f64 {
    if start == 0 {
        return 0.0;
    }
    (end as f64 - start as f64) / start as f64 * 100.0
}

fn print_report(start_rss: u64, end_rss: u64, cycles: u64, samples: &[(u64, u64)]) {
    println!();
    println!("# v4.16 soak audit report");
    println!();
    println!("- cycles        : {cycles}");
    println!("- start RSS     : {start_rss} KiB");
    println!("- end RSS       : {end_rss} KiB");
    println!("- raw start→end : {:+.1}%", drift_pct(start_rss, end_rss));
    // Skip first ~60s as warm-up (initial allocator commit + index
    // build + page-in). Real leak detection compares the
    // post-warmup RSS to the final RSS. Match the v3.4.2 baseline
    // methodology — drift across the long-run phase only.
    let warmup_secs: u64 = 60;
    let warm_idx = samples.iter().position(|(t, _)| *t >= warmup_secs);
    let (warm_rss, warm_drift) = match warm_idx {
        Some(i) => {
            let r = samples[i].1;
            (Some(r), Some(drift_pct(r, end_rss)))
        }
        None => (None, None),
    };
    if let (Some(rss), Some(d)) = (warm_rss, warm_drift) {
        println!("- post-warmup RSS (t=60s): {rss} KiB");
        println!("- post-warmup→end drift  : {d:+.1}%");
    }
    println!();
    println!("| t (s) | RSS (KiB) |");
    println!("|------:|----------:|");
    for (t, r) in samples {
        println!("| {t:>4} | {r:>9} |");
    }
    println!();
    // Verdict uses post-warmup drift when available, raw otherwise.
    let drift = warm_drift.map_or_else(|| drift_pct(start_rss, end_rss).abs(), f64::abs);
    if drift < 2.0 {
        println!("verdict: ✅ leak-free (drift < 2% threshold, v3.4.2 baseline was 0.2%)");
    } else if drift < 10.0 {
        println!("verdict: ⚠️  measurable drift ({drift:.1}%) — investigate before prod");
    } else {
        println!("verdict: ❌ LEAK ({drift:.1}% post-warmup RSS growth)");
        std::process::exit(2);
    }
}

fn spawn_spg_server() -> Result<Child, Box<dyn std::error::Error>> {
    let build = Command::new("cargo")
        .args(["build", "--release", "-q", "-p", "spg-server"])
        .status()?;
    if !build.success() {
        return Err("cargo build spg-server failed".into());
    }
    let target_dir = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".into());
    let bin = format!("{target_dir}/release/spg-server");
    let child = Command::new(&bin)
        .arg(SPG_NATIVE)
        .env("SPG_HTTP_ADDR", SPG_HTTP)
        .env_remove("SPG_PASSWORD")
        .env_remove("SPG_ADMIN_PASSWORD")
        // Inherit stderr so soak crashes / panics are visible inline.
        // (Previously we piped stderr to scrape "listening on"; the
        // OS pipe buffer filled mid-run and stalled the server.)
        .stderr(Stdio::inherit())
        .stdout(Stdio::inherit())
        .spawn()?;
    // Poll for the listener instead of scraping stderr.
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if TcpStream::connect(SPG_NATIVE).is_ok() {
            return Ok(child);
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    Err("spg-server didn't accept connections in 5s".into())
}
