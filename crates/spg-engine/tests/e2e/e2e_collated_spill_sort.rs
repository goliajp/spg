//! v7.38.22 — a collation reaches the SPILLING sort, which it never did.
//!
//! Two sort implementations serve ORDER BY. The materialising one has
//! honoured a declared collation since v7.38.18; the streaming/spilling
//! one compared with an empty collation slice at every site, so a plain
//! single-table `SELECT … ORDER BY s COLLATE "en_US.utf8"` — the shape
//! that takes it — was ordered by BYTES.
//!
//! Measured on the published images rather than reasoned about:
//! 7.38.6, 7.38.16, 7.38.18 and 7.38.21 all answer
//! `Banana, Cherry, apple, date` where PostgreSQL 18.4 answers
//! `apple, Banana, Cherry, date`. Same query, two orders, decided by
//! which path the planner took — and the wrong one is silent.

use spg_engine::{CancelToken, Engine, QueryResult, StreamItem, TempRun, TempStoreError};
use spg_storage::Value;

/// A spill run backed by a `Vec`. The streaming sort declines outright
/// unless the engine can spill, and the first version of this file
/// passed with the fix reverted for exactly that reason — the pin never
/// reached the path it was written for.
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

/// An engine that CAN spill, so the streaming sort is reachable.
fn spilling_engine() -> Engine {
    let mut e = Engine::new();
    e.set_temp_run_factory(mem_run);
    assert!(e.can_spill(), "the path under test declines without this");
    e
}

/// Every first cell the sorted stream emits, in emission order.
fn streamed(e: &Engine, sql: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    e.execute_readonly_select_streaming(sql, CancelToken::none(), |item| {
        if let StreamItem::Row(cells) = item {
            out.push(match cells.get(0).expect("row has a first cell") {
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

fn col0_text(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match &r.values[0] {
                Value::Text(t) => t.to_string(),
                other => panic!("expected text, got {other:?}"),
            })
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

/// `apple` before `Banana` is the collation's answer; bytes put every
/// capital first. PostgreSQL 18.4 gives the first for this query and the
/// second when the COLLATE is dropped, and so must this.
#[test]
fn an_order_by_collate_reaches_the_spilling_sort() {
    let mut e = spilling_engine();
    e.execute("CREATE TABLE t (s TEXT)").unwrap();
    e.execute("INSERT INTO t VALUES ('Banana'), ('apple'), ('Cherry'), ('date')")
        .unwrap();
    assert_eq!(
        streamed(&e, r#"SELECT s FROM t ORDER BY s COLLATE "en_US.utf8""#),
        ["apple", "Banana", "Cherry", "date"],
        "the collation must decide the order on this path too"
    );
    // And without it, nothing changed: a `C` database still orders bytes.
    assert_eq!(
        streamed(&e, "SELECT s FROM t ORDER BY s"),
        ["Banana", "Cherry", "apple", "date"],
        "no collation named, no collation applied"
    );
}

/// The same path swallowed a collation NAME it cannot perform, which is
/// the defect v7.38.18 closed on the other path: a name PostgreSQL does
/// not have must raise, not be ignored.
#[test]
fn an_unknown_collation_name_still_raises_on_this_path() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (s TEXT)").unwrap();
    e.execute("INSERT INTO t VALUES ('b'), ('a')").unwrap();
    let err = e
        .execute(r#"SELECT s FROM t ORDER BY s COLLATE "zz_NOT_A_COLLATION""#)
        .expect_err("an unknown collation must not be ignored");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("zz_NOT_A_COLLATION"),
        "the message must name what was refused: {msg}"
    );
}

/// And a type that cannot carry a collation refuses it, with
/// PostgreSQL's wording. Measured: PG 18.4 answers
/// `ERROR: 42804: collations are not supported by type integer`.
#[test]
fn a_collation_on_a_non_collatable_type_is_refused() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (n INT, b BYTEA)").unwrap();
    e.execute(r"INSERT INTO t VALUES (2, '\x01'), (1, '\x02')")
        .unwrap();
    for (col, ty) in [("n", "integer"), ("b", "bytea")] {
        let err = e
            .execute(&format!(
                r#"SELECT {col} FROM t ORDER BY {col} COLLATE "en_US.utf8""#
            ))
            .expect_err("a non-collatable type must refuse a collation");
        let msg = format!("{err:?}");
        assert!(
            msg.contains(&format!("collations are not supported by type {ty}")),
            "PostgreSQL's wording, naming the type: {msg}"
        );
    }
}

/// The DDL entrance takes the same rule: PostgreSQL refuses
/// `CREATE TABLE t (c INT COLLATE "en_US.utf8")` and SPG stored it.
#[test]
fn a_column_declaring_a_collation_it_cannot_carry_is_refused() {
    let mut e = Engine::new();
    let err = e
        .execute(r#"CREATE TABLE t (c INT COLLATE "en_US.utf8")"#)
        .expect_err("an integer column cannot carry a collation");
    assert!(
        format!("{err:?}").contains("collations are not supported by type integer"),
        "{err:?}"
    );
    // The collatable ones still work, unchanged.
    e.execute(r#"CREATE TABLE ok (c TEXT COLLATE "en_US.utf8")"#)
        .expect("text carries a collation");
}
