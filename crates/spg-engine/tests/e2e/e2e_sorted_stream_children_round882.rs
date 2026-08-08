//! r882 — a sorted stream over a parent returns its children's rows.
//!
//! `try_spill_sorted_stream` claims a single-table `ORDER BY` before the
//! streaming dispatcher's own gates and scans the named relation alone.
//! A partitioned parent holds no rows of its own, so the walk answered
//! with nothing at all and the client saw an empty, perfectly ordered
//! result — a silent short answer, not an error.
//!
//! The differential corpus caught it (`13-partition-inherit` went from 2
//! differing lines to 7). This pins it directly, because a corpus
//! baseline can be re-blessed and this shape should not be.
//!
//! Both mechanisms are covered: declarative partitions, where the parent
//! declares the relationship, and inheritance, where only the children
//! record it — `has_children` is the predicate that answers for both,
//! and it is what the FROM-clause fan-out itself asks.
//!
//! `ONLY` is the case that legitimately does not fan out, and it is
//! pinned too: gating on the parent alone would have made
//! `SELECT ... FROM ONLY parent` decline a walk it is entitled to.

use spg_engine::{CancelToken, Engine, StreamItem, TempRun, TempStoreError};
use spg_storage::Value;

/// A spill run backed by a Vec. The walk under test declines outright
/// unless the engine can spill, so without a factory these tests pass
/// whatever the walk does — which is how the first version of this file
/// passed with its gate deliberately removed.
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

/// An engine that CAN spill, so the sorted-stream walk is reachable.
fn engine() -> Engine {
    let mut e = Engine::new();
    e.set_temp_run_factory(mem_run);
    assert!(e.can_spill(), "the walk under test declines without this");
    e
}

/// Every value the sorted stream emits for `sql`, in the order it
/// emitted them.
fn streamed(e: &Engine, sql: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    e.execute_readonly_select_streaming(sql, CancelToken::none(), |item| {
        if let StreamItem::Row(cells) = item {
            out.push(match cells[0] {
                Value::Text(s) => s.to_string(),
                Value::Int(n) => n.to_string(),
                other => panic!("unexpected cell {other:?}"),
            });
        }
        Ok(())
    })
    .expect("streaming select");
    out
}

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

#[test]
fn a_sorted_stream_over_a_partitioned_parent_sees_every_partition() {
    let mut e = engine();
    ok(
        &mut e,
        "CREATE TABLE pl (k TEXT, v INT) PARTITION BY LIST (k)",
    );
    ok(
        &mut e,
        "CREATE TABLE pla PARTITION OF pl FOR VALUES IN ('a')",
    );
    ok(&mut e, "CREATE TABLE pld PARTITION OF pl DEFAULT");
    ok(&mut e, "INSERT INTO pl VALUES ('z',2),('a',1),('m',3)");

    assert_eq!(
        streamed(&e, "SELECT k FROM pl ORDER BY k"),
        alloc_vec(&["a", "m", "z"]),
        "the parent holds no rows itself; scanning it alone answers empty"
    );
}

#[test]
fn a_sorted_stream_over_an_inheritance_parent_sees_the_children() {
    let mut e = engine();
    ok(&mut e, "CREATE TABLE par (id INT)");
    ok(&mut e, "CREATE TABLE kid () INHERITS (par)");
    ok(&mut e, "INSERT INTO par VALUES (2)");
    ok(&mut e, "INSERT INTO kid VALUES (1),(3)");

    // An inheritance parent keeps rows of its own, so a walk that scans
    // it alone returns SOME rows — which is why this cannot be caught by
    // checking merely that the answer is non-empty.
    assert_eq!(
        streamed(&e, "SELECT id FROM par ORDER BY id"),
        alloc_vec(&["1", "2", "3"]),
    );
}

#[test]
fn only_still_takes_the_walk_and_still_answers_the_parent_alone() {
    let mut e = engine();
    ok(&mut e, "CREATE TABLE par2 (id INT)");
    ok(&mut e, "CREATE TABLE kid2 () INHERITS (par2)");
    ok(&mut e, "INSERT INTO par2 VALUES (2)");
    ok(&mut e, "INSERT INTO kid2 VALUES (1),(3)");

    assert_eq!(
        streamed(&e, "SELECT id FROM ONLY par2 ORDER BY id"),
        alloc_vec(&["2"]),
        "ONLY does not fan out, so the child's rows must NOT appear"
    );
}

fn alloc_vec(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}
