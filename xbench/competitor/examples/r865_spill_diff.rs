//! r865 — the spilled sort must answer exactly what the in-memory one
//! does. Same statements, `work_mem` set so one spills and the other
//! does not, compared row for row.
use spg_engine::{Engine, QueryResult, TempRun, TempStoreError};

/// A run in memory. The engine takes any `TempRun`; the server hands it
/// a file, and this keeps the differential to one process without one.
#[derive(Default)]
struct MemRun {
    buf: Vec<u8>,
    at: usize,
}

impl TempRun for MemRun {
    fn append(&mut self, bytes: &[u8]) -> Result<(), TempStoreError> {
        self.buf.extend_from_slice(bytes);
        Ok(())
    }
    fn seal(&mut self) -> Result<(), TempStoreError> {
        self.at = 0;
        Ok(())
    }
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, TempStoreError> {
        let n = (self.buf.len() - self.at).min(buf.len());
        buf[..n].copy_from_slice(&self.buf[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
    fn bytes_written(&self) -> u64 {
        self.buf.len() as u64
    }
}

/// How many runs the factory has handed out. If this is zero the two
/// engines ran the SAME path and the comparison below proves nothing.
static RUNS_OPENED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl MemRun {
    fn counted() -> Self {
        RUNS_OPENED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self::default()
    }
}

const SHAPES: &[(&str, &str)] = &[
    ("plain", "SELECT pad FROM big ORDER BY id"),
    ("desc", "SELECT pad FROM big ORDER BY id DESC"),
    ("two_keys", "SELECT pad FROM big ORDER BY g, id"),
    ("mixed_dir", "SELECT pad FROM big ORDER BY g DESC, id ASC"),
    ("expr_key", "SELECT pad FROM big ORDER BY id % 100, id"),
    (
        "where_filtered",
        "SELECT pad FROM big WHERE id % 3 = 0 ORDER BY id",
    ),
    ("proj_expr", "SELECT id * 2, pad FROM big ORDER BY id"),
    ("nulls", "SELECT pad FROM big ORDER BY g NULLS FIRST, id"),
];

fn rows_of(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| r.values.iter().map(|v| format!("{v:?}")).collect())
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn seed(e: &mut Engine, n: usize) {
    use std::fmt::Write as _;
    e.execute("CREATE TABLE big (id INT PRIMARY KEY, g INT, pad TEXT)")
        .unwrap();
    for chunk in 0..(n / 1000) {
        let mut sql = String::from("INSERT INTO big VALUES ");
        for k in 0..1000 {
            let i = chunk * 1000 + k;
            if k > 0 {
                sql.push(',');
            }
            let g = if i % 17 == 0 {
                "NULL".into()
            } else {
                format!("{}", i % 50)
            };
            write!(sql, "({i},{g},'{}')", "y".repeat(200)).unwrap();
        }
        e.execute(&sql).unwrap();
    }
}

fn main() {
    const N: usize = 40_000;
    let mut spilled = Engine::new();
    spilled.set_temp_run_factory(|| Ok(Box::new(MemRun::counted())));
    seed(&mut spilled, N);
    spilled.execute("SET work_mem = '1024kB'").unwrap();

    let mut memory = Engine::new();
    seed(&mut memory, N);
    memory.execute("SET work_mem = '512MB'").unwrap();

    let mut bad = 0;
    for (name, sql) in SHAPES {
        let a = rows_of(&mut spilled, sql);
        let b = rows_of(&mut memory, sql);
        let same = a == b;
        if !same {
            bad += 1;
        }
        println!(
            "{:<16} rows={:<6} {}",
            name,
            a.len(),
            if same { "identical" } else { "DIFFERENT" }
        );
    }
    let runs = RUNS_OPENED.load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "{} of {} shapes differ; runs opened = {runs}",
        bad,
        SHAPES.len()
    );
    assert_eq!(bad, 0);
    // Without this the comparison is vacuous: two engines on the same
    // path agree trivially.
    assert!(runs > 0, "nothing spilled — the gate never fired");
}
