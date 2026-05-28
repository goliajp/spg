//! v5.1: hand-bake a cold-tier segment for the `sweep` schema so
//! a sweep run can preload it into spg-server and exercise the
//! v5.1 cold-tier read path on a 30M-row corpus.
//!
//! Schema mirrors `sweep`'s exactly:
//!
//!   CREATE TABLE sweep (id INT NOT NULL, sec INT NOT NULL, name TEXT NOT NULL)
//!
//! Each row is dense-encoded (FILE_VERSION 8) so
//! `Catalog::resolve_cold_locator` round-trips to the same `Row`
//! the server-side INSERT path would have produced. The `sec` and
//! `name` columns follow sweep's per-row formula so a SELECT WHERE
//! id = X returns the same payload regardless of whether the row
//! was inserted hot or preloaded cold.
//!
//! Usage:
//!
//!   cargo run --release -p spg-bench-competitor --bin bake_segment -- \
//!     --rows 30000000 --output /tmp/sweep_30m.spg
//!
//! Defaults: 30_000_000 rows, output to `/tmp/sweep_<rows>.spg`.
//! The 30M default matches the v5.1 → v5.2 ship-gate corpus size.

use std::path::PathBuf;
use std::time::Instant;

use spg_storage::{
    ColumnSchema, DataType, Row, SEGMENT_PAGE_BYTES, TableSchema, Value, encode_row_body_dense,
    encode_segment,
};

const DEFAULT_ROWS: u64 = 30_000_000;

/// Same schema string the sweep harness's `CREATE TABLE sweep ...`
/// produces. We rebuild it here as a `TableSchema` value so
/// `encode_row_body_dense` lays out cells in the same wire order
/// as the server's `Catalog::serialize`.
fn sweep_schema() -> TableSchema {
    TableSchema::new(
        "sweep",
        vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new("sec", DataType::Int, false),
            ColumnSchema::new("name", DataType::Text, false),
        ],
    )
}

/// Compute one row's `(id, sec, name)` per sweep's INSERT formula.
/// `sec = (id * 2654435761 % 1_000_000_000) as i32` (Fibonacci-
/// hash spread); `name = "u-{id}"`. Keeps cold-tier rows
/// byte-identical to hot-tier rows so SELECTs don't care.
fn make_sweep_row(id: i32) -> Row {
    let sec = ((id as u64).wrapping_mul(2_654_435_761) % 1_000_000_000) as i32;
    let name = format!("u-{id}");
    Row::new(vec![
        Value::Int(id),
        Value::Int(sec),
        Value::Text(name),
    ])
}

fn parse_args() -> (u64, PathBuf) {
    let mut rows = DEFAULT_ROWS;
    let mut output: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--rows" => {
                let v = args.next().expect("--rows takes a value");
                rows = v.parse::<u64>().expect("--rows must be a number");
                assert!(rows > 0, "--rows must be > 0");
                assert!(
                    rows <= u64::from(u32::MAX),
                    "--rows must be ≤ u32::MAX (segment header is u32)"
                );
            }
            "--output" => {
                let v = args.next().expect("--output takes a path");
                output = Some(PathBuf::from(v));
            }
            "--help" | "-h" => {
                eprintln!(
                    "bake_segment: hand-bake a cold-tier segment for sweep\n\
                     \n\
                     Usage:\n  \
                       cargo run --release -p spg-bench-competitor --bin bake_segment -- \\\n  \
                                  [--rows N] [--output PATH]\n"
                );
                std::process::exit(0);
            }
            other => panic!("unknown arg: {other}"),
        }
    }
    let output = output.unwrap_or_else(|| PathBuf::from(format!("/tmp/sweep_{rows}.spg")));
    (rows, output)
}

/// Streaming, ExactSizeIterator-compatible iterator over the
/// `(u64 key, dense row body)` pairs `encode_segment` consumes.
/// Avoids the multi-GB intermediate `Vec` we'd need if we
/// `(1..=N).map(...).collect()`-ed.
struct SweepRows<'a> {
    schema: &'a TableSchema,
    next: i32,
    end: i32, // inclusive
}

impl Iterator for SweepRows<'_> {
    type Item = (u64, Vec<u8>);
    fn next(&mut self) -> Option<Self::Item> {
        if self.next > self.end {
            return None;
        }
        let id = self.next;
        self.next += 1;
        let row = make_sweep_row(id);
        let body = encode_row_body_dense(&row, self.schema);
        Some((id as u64, body))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = (self.end - self.next + 1).max(0) as usize;
        (n, Some(n))
    }
}

impl ExactSizeIterator for SweepRows<'_> {}

fn main() {
    let (rows, output) = parse_args();
    eprintln!(
        "bake_segment: generating {rows} sweep-schema rows → {}",
        output.display()
    );
    let schema = sweep_schema();
    let t_encode = Instant::now();
    // 32-byte estimate per row body is generous (3 fixed-width
    // cells + short Text "u-N"); the segment writer fits whatever
    // we hand it inside `SEGMENT_PAGE_BYTES` (4 KiB) per page.
    let end = i32::try_from(rows).expect("rows ≤ i32::MAX");
    let seg_iter = SweepRows {
        schema: &schema,
        next: 1,
        end,
    };
    let (bytes, meta) = encode_segment(seg_iter, 0.01, SEGMENT_PAGE_BYTES)
        .expect("encode_segment");
    let encode_secs = t_encode.elapsed().as_secs_f64();
    eprintln!(
        "bake_segment: encoded {} rows in {encode_secs:.2}s = {} pages, {} bytes total",
        meta.num_rows,
        meta.num_pages,
        meta.total_bytes
    );
    let t_write = Instant::now();
    std::fs::write(&output, &bytes).expect("write segment");
    let write_secs = t_write.elapsed().as_secs_f64();
    eprintln!(
        "bake_segment: wrote {} MB in {write_secs:.2}s ({:.0} MB/s)",
        bytes.len() / (1024 * 1024),
        bytes.len() as f64 / (1024.0 * 1024.0) / write_secs.max(1e-9)
    );
    eprintln!(
        "bake_segment: done. \n  \
         start spg-server with: SPG_PRELOAD_COLD_SEGMENT='sweep:sweep_id_idx:{}'",
        output.display()
    );
}
