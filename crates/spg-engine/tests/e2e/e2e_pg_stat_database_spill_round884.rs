//! r884 — `pg_stat_database` answers the database it is in, and counts
//! the spills that really happened.
//!
//! Two defects, both of the shape where a standard query comes back
//! wrong rather than failing:
//!
//!   * `datname` was the literal `spg` while `current_database()`
//!     answered the name the client connected with, so
//!     `... WHERE datname = current_database()` — what every monitoring
//!     dashboard writes — matched no row and returned an EMPTY result.
//!     `pg_database` had been given this treatment in round 474; its
//!     sibling here was missed.
//!   * `temp_files` / `temp_bytes` read 0 while the sorter was writing
//!     26 runs and 86 MB for a single query. PG's pair is what a
//!     dashboard watches to find the queries that outgrow `work_mem`,
//!     so 0 is not a harmless placeholder — it is the signal saying
//!     "nothing spilled" when everything did.
//!
//! The second test's last assertion is the one that carries it: a sort
//! that does NOT spill must leave the counters alone. Without it the
//! test passes on an implementation that counts queries.

use spg_engine::{CancelToken, Engine, StreamItem, TempRun, TempStoreError};
use spg_storage::Value;

struct MemRun {
    buf: Vec<u8>,
    read_at: usize,
}

impl TempRun for MemRun {
    fn append(&mut self, bytes: &[u8]) -> Result<(), TempStoreError> {
        self.buf.extend_from_slice(bytes);
        Ok(())
    }
    fn seal(&mut self) -> Result<(), TempStoreError> {
        self.read_at = 0;
        Ok(())
    }
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, TempStoreError> {
        let n = core::cmp::min(buf.len(), self.buf.len() - self.read_at);
        buf[..n].copy_from_slice(&self.buf[self.read_at..self.read_at + n]);
        self.read_at += n;
        Ok(n)
    }
    fn bytes_written(&self) -> u64 {
        self.buf.len() as u64
    }
}

fn mem_run() -> Result<Box<dyn TempRun>, TempStoreError> {
    Ok(Box::new(MemRun {
        buf: Vec::new(),
        read_at: 0,
    }))
}

fn engine() -> Engine {
    let mut e = Engine::new();
    e.set_temp_run_factory(mem_run);
    assert!(e.can_spill(), "the sorter under test declines without this");
    e
}

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

/// One scalar, as text.
fn scalar(e: &mut Engine, sql: &str) -> String {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        spg_engine::QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "{sql} should answer exactly one row");
            match &rows[0].values[0] {
                Value::Text(s) => s.to_string(),
                Value::BigInt(n) => n.to_string(),
                other => panic!("{sql}: unexpected {other:?}"),
            }
        }
        other => panic!("{sql}: unexpected {other:?}"),
    }
}

/// Drive a query through the streaming path — the one the sorted walk
/// is hooked into — and count the rows, discarding them.
fn stream_rows(e: &Engine, sql: &str) -> usize {
    e.execute_readonly_select_streaming(sql, CancelToken::none(), |item| {
        let _ = matches!(item, StreamItem::Row(_));
        Ok(())
    })
    .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
}

#[test]
fn pg_stat_database_names_the_database_current_database_answers() {
    let mut e = engine();
    ok(&mut e, "SET spg.database = 'app'");

    let current = scalar(&mut e, "SELECT current_database()");
    let datname = scalar(&mut e, "SELECT datname FROM pg_stat_database");
    assert_eq!(datname, current, "the join every dashboard makes");
    assert_eq!(current, "app");

    // And the shape itself, which is what actually broke: this returned
    // no rows at all.
    let joined = scalar(
        &mut e,
        "SELECT datname FROM pg_stat_database WHERE datname = current_database()",
    );
    assert_eq!(joined, "app");
}

#[test]
fn temp_files_and_temp_bytes_count_spills_and_only_spills() {
    let mut e = engine();
    ok(&mut e, "CREATE TABLE big (id INT, k INT, pad TEXT)");
    for chunk in 0..40 {
        let mut sql = alloc_string("INSERT INTO big VALUES ");
        for i in 0..500 {
            let n = chunk * 500 + i;
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&format!(
                "({n},{},'{}')",
                (n * 7919) % 20000,
                "y".repeat(64)
            ));
        }
        ok(&mut e, &sql);
    }

    let files0 = scalar(&mut e, "SELECT temp_files FROM pg_stat_database");
    let bytes0 = scalar(&mut e, "SELECT temp_bytes FROM pg_stat_database");
    assert_eq!((files0.as_str(), bytes0.as_str()), ("0", "0"));

    // A budget this small cannot hold 20k rows of 64-byte padding.
    ok(&mut e, "SET work_mem = 64");
    assert_eq!(stream_rows(&e, "SELECT pad FROM big ORDER BY k"), 20_000);

    let files1: u64 = scalar(&mut e, "SELECT temp_files FROM pg_stat_database")
        .parse()
        .unwrap();
    let bytes1: u64 = scalar(&mut e, "SELECT temp_bytes FROM pg_stat_database")
        .parse()
        .unwrap();
    assert!(files1 > 0, "the sort spilled; temp_files stayed {files1}");
    assert!(bytes1 > 0, "the sort spilled; temp_bytes stayed {bytes1}");

    // The discriminating half: the same sort with room to run must not
    // touch either counter. A counter that ticks per query passes
    // everything above and fails here.
    ok(&mut e, "SET work_mem = 1048576");
    assert_eq!(stream_rows(&e, "SELECT pad FROM big ORDER BY k"), 20_000);

    let files2: u64 = scalar(&mut e, "SELECT temp_files FROM pg_stat_database")
        .parse()
        .unwrap();
    let bytes2: u64 = scalar(&mut e, "SELECT temp_bytes FROM pg_stat_database")
        .parse()
        .unwrap();
    assert_eq!(
        (files2, bytes2),
        (files1, bytes1),
        "a sort that fits spilled nothing"
    );
}

fn alloc_string(s: &str) -> String {
    s.to_string()
}
