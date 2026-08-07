//! v7.37 (round 833) — T35 Phase B: sort more rows than fit in `work_mem`.
//!
//! Sorting held every row twice. The scan materialises `Vec<Row>`, and
//! the sort then builds `Vec<(Vec<OrderKey>, Row)>` beside it, so peak
//! tracked the input with no ceiling anywhere: measured on
//! `SELECT pad FROM big ORDER BY id`, peak RSS 218 MB at 100k rows,
//! 390 MB at 200k, 807 MB at 400k. `work_mem` reads 4 MB and bounded
//! none of it. A large enough ORDER BY takes the server down, which is
//! a liveness problem rather than a performance one.
//!
//! Round 786/787 left the seam for this — `TempRun` is a byte stream the
//! host provides (a file, on the server) — and nothing ever called
//! `can_spill()`. This is the piece that does: fill up to the budget,
//! sort that much, write it out as a sorted RUN, drop it, repeat; then
//! merge the runs back k-way. Peak is then the budget plus one decoded
//! row per run, whatever the input size.
//!
//! Runs store ROWS only, not keys. An `OrderKey` is a rich enum whose
//! encoding would be a second format to keep in step with the first,
//! and the caller already owns the definition of the keys — it hands in
//! a closure, and the merge re-derives them from each decoded row. The
//! cost is one key evaluation per row per merge, paid only when a sort
//! actually spills.
//!
//! With no factory (an embedded engine with nowhere to put a file) this
//! degrades to exactly today's behaviour: everything stays in the batch
//! and is sorted in memory. That is a ceiling the host opts into, not
//! one imposed on it.

// Phase B's core, unit-tested and not yet on any query path — the same
// shape Phase A shipped `TempRun` in (round 786). The wiring is a
// separate surgery: the sites that sort today receive a `Vec<Row>` that
// the scan has already materialised, so handing it to this changes
// nothing about peak memory. What has to move first is row PRODUCTION,
// so rows reach the sorter one at a time; until then, wiring it in
// would buy a `Sort Method: external merge` line and no ceiling, which
// is the kind of half-measure that reads as done.
#![allow(dead_code)]

use alloc::boxed::Box;
use alloc::vec::Vec;

use spg_storage::{Row, TableSchema};

use crate::EngineError;
use crate::orderby::{OrderKey, cmp_multi_key_in};
use crate::tempstore::TempRun;

/// One sorted run on the host's temp storage, plus the read cursor the
/// merge walks it with.
struct RunReader {
    run: Box<dyn TempRun>,
    /// The row at this run's head, and its keys. `None` once drained.
    head: Option<(Vec<OrderKey>, Row<'static>)>,
}

/// Read exactly `n` bytes, or `None` at a clean end of stream.
fn read_exact(run: &mut dyn TempRun, n: usize) -> Result<Option<Vec<u8>>, EngineError> {
    let mut buf = alloc::vec![0u8; n];
    let mut filled = 0;
    while filled < n {
        let got = run
            .read(&mut buf[filled..])
            .map_err(|e| EngineError::Internal(alloc::format!("temp run read: {e:?}")))?;
        if got == 0 {
            if filled == 0 {
                return Ok(None);
            }
            return Err(EngineError::Internal(alloc::string::String::from(
                "temp run ended mid-record",
            )));
        }
        filled += got;
    }
    Ok(Some(buf))
}

/// Accumulate rows, spilling sorted runs once the budget is reached.
pub(crate) struct ExternalSorter<'a> {
    factory: Option<crate::TempRunFactory>,
    budget_bytes: usize,
    schema: TableSchema,
    descs: &'a [bool],
    batch: Vec<(Vec<OrderKey>, Row<'static>)>,
    batch_bytes: usize,
    runs: Vec<Box<dyn TempRun>>,
}

impl<'a> ExternalSorter<'a> {
    pub(crate) fn new(
        factory: Option<crate::TempRunFactory>,
        budget_bytes: usize,
        schema: TableSchema,
        descs: &'a [bool],
    ) -> Self {
        Self {
            factory,
            budget_bytes,
            schema,
            descs,
            batch: Vec::new(),
            batch_bytes: 0,
            runs: Vec::new(),
        }
    }

    /// Did this sort actually spill? Pins read it; so does the decision
    /// to report temp bytes.
    pub(crate) fn spilled(&self) -> bool {
        !self.runs.is_empty()
    }

    pub(crate) fn push(
        &mut self,
        keys: Vec<OrderKey>,
        row: Row<'static>,
    ) -> Result<(), EngineError> {
        self.batch_bytes += crate::bytebudget::approx_values_bytes(&row.values);
        self.batch.push((keys, row));
        // Spilling needs somewhere to spill to. Without a factory this
        // is the old unbounded behaviour, deliberately.
        if self.factory.is_some() && self.batch_bytes >= self.budget_bytes {
            self.spill_batch()?;
        }
        Ok(())
    }

    /// Sort what is held and write it out as one run, freeing the rows.
    fn spill_batch(&mut self) -> Result<(), EngineError> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let Some(factory) = self.factory else {
            return Ok(());
        };
        crate::orderby::sort_by_keys(&mut self.batch, self.descs);
        let mut run = factory()
            .map_err(|e| EngineError::Internal(alloc::format!("temp run create: {e:?}")))?;
        for (_, row) in core::mem::take(&mut self.batch) {
            let bytes = spg_storage::encode_row_body_dense(&row, &self.schema);
            let len = u32::try_from(bytes.len()).map_err(|_| {
                EngineError::Internal(alloc::string::String::from("row too large to spill"))
            })?;
            run.append(&len.to_le_bytes())
                .map_err(|e| EngineError::Internal(alloc::format!("temp run append: {e:?}")))?;
            run.append(&bytes)
                .map_err(|e| EngineError::Internal(alloc::format!("temp run append: {e:?}")))?;
        }
        run.seal()
            .map_err(|e| EngineError::Internal(alloc::format!("temp run seal: {e:?}")))?;
        self.runs.push(run);
        self.batch_bytes = 0;
        Ok(())
    }

    /// Every row, in order. `keys_of` re-derives a spilled row's keys —
    /// the same function the caller used on the way in, so a run needs
    /// to carry only the row.
    pub(crate) fn finish<K>(mut self, keys_of: K) -> Result<Vec<Row<'static>>, EngineError>
    where
        K: Fn(&Row<'static>) -> Result<Vec<OrderKey>, EngineError>,
    {
        if self.runs.is_empty() {
            // Never spilled: one in-memory sort, exactly as before.
            crate::orderby::sort_by_keys(&mut self.batch, self.descs);
            return Ok(self.batch.into_iter().map(|(_, r)| r).collect());
        }
        // Whatever is still held becomes the last run, so the merge has
        // one kind of input rather than two.
        self.spill_batch()?;

        let mut readers: Vec<RunReader> = Vec::with_capacity(self.runs.len());
        for mut run in core::mem::take(&mut self.runs) {
            let head = Self::next_row(&mut *run, &self.schema, &keys_of)?;
            readers.push(RunReader { run, head });
        }

        let mut out: Vec<Row<'static>> = Vec::new();
        loop {
            // Pick the run whose head sorts first. Linear in the number
            // of runs, which is the input divided by the budget — a heap
            // buys nothing at the counts this produces.
            let mut best: Option<usize> = None;
            for (i, r) in readers.iter().enumerate() {
                let Some((keys, _)) = &r.head else { continue };
                match best {
                    None => best = Some(i),
                    Some(b) => {
                        let (bk, _) = readers[b].head.as_ref().expect("checked above");
                        if cmp_multi_key_in(keys, bk, self.descs, &[]) == core::cmp::Ordering::Less {
                            best = Some(i);
                        }
                    }
                }
            }
            let Some(i) = best else { break };
            let (_, row) = readers[i].head.take().expect("chosen head is present");
            out.push(row);
            readers[i].head = Self::next_row(&mut *readers[i].run, &self.schema, &keys_of)?;
        }
        Ok(out)
    }

    fn next_row<K>(
        run: &mut dyn TempRun,
        schema: &TableSchema,
        keys_of: &K,
    ) -> Result<Option<(Vec<OrderKey>, Row<'static>)>, EngineError>
    where
        K: Fn(&Row<'static>) -> Result<Vec<OrderKey>, EngineError>,
    {
        let Some(len_bytes) = read_exact(run, 4)? else {
            return Ok(None);
        };
        let len = u32::from_le_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]);
        let Some(body) = read_exact(run, len as usize)? else {
            return Err(EngineError::Internal(alloc::string::String::from(
                "temp run ended before its row body",
            )));
        };
        let (row, _) = spg_storage::decode_row_body_dense(
            &body,
            schema,
            spg_storage::CURRENT_ROW_CODEC_VERSION,
        )
        .map_err(|e| EngineError::Internal(alloc::format!("temp run row decode: {e:?}")))?;
        let keys = keys_of(&row)?;
        Ok(Some((keys, row)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use spg_storage::{ColumnSchema, DataType, Value};

    /// A run backed by a Vec, so the sorter's own logic can be tested
    /// without a host that owns files.
    struct MemRun {
        buf: Vec<u8>,
        read_at: usize,
        sealed: bool,
    }

    impl TempRun for MemRun {
        fn append(&mut self, bytes: &[u8]) -> Result<(), crate::tempstore::TempStoreError> {
            assert!(!self.sealed, "append after seal");
            self.buf.extend_from_slice(bytes);
            Ok(())
        }
        fn seal(&mut self) -> Result<(), crate::tempstore::TempStoreError> {
            self.sealed = true;
            self.read_at = 0;
            Ok(())
        }
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, crate::tempstore::TempStoreError> {
            let n = core::cmp::min(buf.len(), self.buf.len() - self.read_at);
            buf[..n].copy_from_slice(&self.buf[self.read_at..self.read_at + n]);
            self.read_at += n;
            Ok(n)
        }
        fn bytes_written(&self) -> u64 {
            self.buf.len() as u64
        }
    }

    fn mem_run() -> Result<Box<dyn TempRun>, crate::tempstore::TempStoreError> {
        Ok(Box::new(MemRun {
            buf: Vec::new(),
            read_at: 0,
            sealed: false,
        }))
    }

    fn schema() -> TableSchema {
        TableSchema::new(
            "t",
            alloc::vec![
                ColumnSchema::new("id", DataType::Int, false),
                ColumnSchema::new("pad", DataType::Text, false),
            ],
        )
    }

    fn row(id: i32) -> Row<'static> {
        Row::new(alloc::vec![
            Value::Int(id),
            Value::text(alloc::format!("pad-{id}")),
        ])
    }

    fn key_of(r: &Row<'static>) -> Result<Vec<OrderKey>, EngineError> {
        match &r.values[0] {
            Value::Int(n) => Ok(alloc::vec![OrderKey::Int(i128::from(*n))]),
            other => Err(EngineError::Internal(alloc::format!("bad key: {other:?}"))),
        }
    }

    /// Shuffled input, deliberately tiny budget: the answer must be the
    /// sorted one AND the sorter must actually have spilled to get it.
    /// Without the second half this passes just as well when nothing
    /// reaches disk, which is the only thing it is here to prove.
    #[test]
    fn a_spilled_sort_returns_exactly_what_an_in_memory_sort_would() {
        let descs = [false];
        let mut s = ExternalSorter::new(Some(mem_run), 1, schema(), &descs);
        let order = [7, 3, 9, 1, 8, 2, 6, 4, 5, 0];
        for id in order {
            let r = row(id);
            let k = key_of(&r).unwrap();
            s.push(k, r).unwrap();
        }
        assert!(s.spilled(), "a 1-byte budget must have produced runs");
        let out = s.finish(key_of).unwrap();
        let ids: Vec<i32> = out
            .iter()
            .map(|r| match r.values[0] {
                Value::Int(n) => n,
                _ => panic!("id column"),
            })
            .collect();
        assert_eq!(ids, alloc::vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        // The payload survives the round trip through the run, not just
        // the key it was sorted on.
        assert_eq!(
            out[0].values[1],
            Value::text("pad-0".to_string()),
            "the whole row comes back, not only its sort key"
        );
    }

    #[test]
    fn descending_order_survives_the_merge() {
        let descs = [true];
        let mut s = ExternalSorter::new(Some(mem_run), 1, schema(), &descs);
        for id in [3, 1, 2] {
            let r = row(id);
            let k = key_of(&r).unwrap();
            s.push(k, r).unwrap();
        }
        assert!(s.spilled());
        let ids: Vec<i32> = s
            .finish(key_of)
            .unwrap()
            .iter()
            .map(|r| match r.values[0] {
                Value::Int(n) => n,
                _ => panic!("id column"),
            })
            .collect();
        assert_eq!(ids, alloc::vec![3, 2, 1], "DESC must survive the k-way merge");
    }

    /// No factory: the old behaviour, byte for byte. A host with nowhere
    /// to spill must not start failing sorts it used to answer.
    #[test]
    fn without_a_factory_the_sort_stays_in_memory_and_still_sorts() {
        let descs = [false];
        let mut s = ExternalSorter::new(None, 1, schema(), &descs);
        for id in [2, 0, 1] {
            let r = row(id);
            let k = key_of(&r).unwrap();
            s.push(k, r).unwrap();
        }
        assert!(!s.spilled(), "nothing to spill to means nothing spilled");
        let ids: Vec<i32> = s
            .finish(key_of)
            .unwrap()
            .iter()
            .map(|r| match r.values[0] {
                Value::Int(n) => n,
                _ => panic!("id column"),
            })
            .collect();
        assert_eq!(ids, alloc::vec![0, 1, 2]);
    }
}
