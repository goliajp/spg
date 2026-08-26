//! v7.38.23 — the collated sort that SPILLS answers what the collated
//! sort that fits answers.
//!
//! v7.38.22 shipped for the defect that a declared collation reached
//! only one of the two sort paths, and the corpus pin written for it
//! holds FOUR rows and never sets `work_mem`. Four rows fit anywhere, so
//! the pin exercised the materialising sort twice and the spilling sort
//! never — the pin for "a collation reaches BOTH paths" could not reach
//! the second one. `e2e_pg_stat_database_spill_round884` already knew
//! the shape that spills: 20,000 rows of 64-byte text under
//! `work_mem = 64`. This is that shape with a collation on it.
//!
//! The last assertion is the one that carries it. Two paths agreeing
//! proves nothing on its own — they agree when both order by bytes,
//! which is exactly what every published SPG through 7.38.21 did. So the
//! answer is also checked against the collation: `apple` sorts before
//! `Banana` under `en_US.utf8` and after it under byte order, and the
//! first row says which one ran.

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

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn counter(e: &mut Engine, which: &str) -> u64 {
    let sql = format!("SELECT {which} FROM pg_stat_database");
    match e.execute(&sql).unwrap_or_else(|err| panic!("{sql}: {err:?}")) {
        spg_engine::QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::BigInt(n) => u64::try_from(*n).unwrap(),
            other => panic!("{sql}: unexpected {other:?}"),
        },
        other => panic!("{sql}: unexpected {other:?}"),
    }
}

/// Every value the query emits, in the order it emits them.
fn stream_texts(e: &Engine, sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    e.execute_readonly_select_streaming(sql, CancelToken::none(), |item| {
        if let StreamItem::Row(r) = item {
            if let Some(Value::Text(s)) = r.get(0) {
                out.push(s.to_string());
            }
        }
        Ok(())
    })
    .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    out
}

const ROWS: usize = 20_000;
const Q: &str = r#"SELECT s FROM coll_spill ORDER BY s COLLATE "en_US.utf8""#;

fn loaded() -> Engine {
    let mut e = Engine::new();
    e.set_temp_run_factory(mem_run);
    assert!(e.can_spill(), "the sorter under test declines without this");
    ok(&mut e, "CREATE TABLE coll_spill (id INT, s TEXT)");
    // The four words differ in case, so byte order and `en_US.utf8`
    // disagree about them: bytes give Banana, Cherry, apple, date and
    // the collation gives apple, Banana, Cherry, date. The padding is
    // what makes 20,000 rows outgrow a 64 kB budget.
    let words = ["apple", "Banana", "Cherry", "date"];
    for chunk in 0..40 {
        let mut sql = String::from("INSERT INTO coll_spill VALUES ");
        for i in 0..500 {
            let n = chunk * 500 + i;
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&format!(
                "({n},'{}{n:06}{}')",
                words[n % 4],
                "y".repeat(50)
            ));
        }
        ok(&mut e, &sql);
    }
    e
}

#[test]
fn a_collated_sort_that_spills_answers_what_one_that_fits_answers() {
    let mut e = loaded();

    let files0 = counter(&mut e, "temp_files");
    ok(&mut e, "SET work_mem = 64");
    let spilled = stream_texts(&e, Q);
    let files1 = counter(&mut e, "temp_files");
    assert_eq!(spilled.len(), ROWS);
    assert!(
        files1 > files0,
        "this sort was supposed to spill; temp_files went {files0} -> {files1}. \
         Without that the two halves below are the same path twice."
    );

    ok(&mut e, "SET work_mem = 1048576");
    let in_memory = stream_texts(&e, Q);
    let files2 = counter(&mut e, "temp_files");
    assert_eq!(in_memory.len(), ROWS);
    assert_eq!(files2, files1, "this one was supposed to fit");

    // The first place they part, and nothing else. Printing 20,000
    // values buries the one fact the reader needs under 2.5 MB.
    if let Some(i) = (0..ROWS).find(|&i| spilled[i] != in_memory[i]) {
        panic!(
            "one query, two paths, two answers — v7.38.22's defect.\n               first divergence at row {i}\n               spilled:   {}\n  in memory: {}",
            spilled[i], in_memory[i]
        );
    }

    // And the half that keeps two wrong answers from agreeing: bytes put
    // `Banana` first, the collation puts `apple` first.
    assert!(
        spilled[0].starts_with("apple"),
        "ordered by BYTES, not by the collation: first value {}",
        spilled[0]
    );
    assert!(
        in_memory[0].starts_with("apple"),
        "ordered by BYTES, not by the collation: first value {}",
        in_memory[0]
    );
}
