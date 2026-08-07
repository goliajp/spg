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
//! ⚠️ **That last paragraph does not survive contact with the real call
//! site, and this module has not been wired because of it** (round 834
//! found it while planning the wiring; recorded here rather than left
//! to be rediscovered).
//!
//! The rows this would spill are PROJECTED, and an ORDER BY key need
//! not appear in the projection: `SELECT pad FROM big ORDER BY id`
//! projects `pad` and sorts on `id`. Re-deriving keys from a decoded
//! projected row is therefore impossible in general, and the closure
//! above can only work when every key happens to be projected.
//!
//! The fix keeps the no-second-format property: spill the key VALUES as
//! leading hidden columns — `key_values ++ projected_values`, under a
//! schema built the same way — and on decode split them off, rebuild the
//! keys with `build_order_keys_bound` over a `Row` of just those values
//! (the shape the SRF branch of `run_single_table_scan` already uses),
//! and hand back the remainder as the output row. Values are what the
//! row codec already encodes, so nothing new has to be kept in step.
//! The unit tests below pass because their key IS a projected column,
//! which is exactly the case that hid this.
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
    /// How many leading values of a spilled record are key values.
    n_keys: usize,
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
    spill_schema: TableSchema,
    n_keys: usize,
    descs: &'a [bool],
    batch: Vec<(Vec<OrderKey>, Vec<spg_storage::Value<'static>>, Row<'static>)>,
    batch_bytes: usize,
    runs: Vec<Box<dyn TempRun>>,
}

impl<'a> ExternalSorter<'a> {
    /// `key_cols` describes the ORDER BY operands, `out_cols` the
    /// projected result. A spilled record is the two concatenated, which
    /// is what lets a key that was never projected survive the trip.
    pub(crate) fn new(
        factory: Option<crate::TempRunFactory>,
        budget_bytes: usize,
        key_cols: Vec<spg_storage::ColumnSchema>,
        out_cols: Vec<spg_storage::ColumnSchema>,
        descs: &'a [bool],
    ) -> Self {
        let n_keys = key_cols.len();
        let mut all = key_cols;
        all.extend(out_cols);
        Self {
            factory,
            budget_bytes,
            spill_schema: TableSchema::new("spg_sort_run", all),
            n_keys,
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

    /// `key_values` are the raw ORDER BY operands for this row, in
    /// clause order. They ride along so a spilled row can be re-keyed
    /// without the key having to be part of the projection — see the
    /// module header.
    pub(crate) fn push(
        &mut self,
        keys: Vec<OrderKey>,
        key_values: Vec<spg_storage::Value<'static>>,
        row: Row<'static>,
    ) -> Result<(), EngineError> {
        self.batch_bytes += crate::bytebudget::approx_values_bytes(&row.values);
        self.batch.push((keys, key_values, row));
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
        // `sort_by_keys` wants (keys, payload) pairs; the key values ride
        // in the payload so the sort is unchanged by their presence.
        let mut pairs: Vec<(Vec<OrderKey>, (Vec<spg_storage::Value<'static>>, Row<'static>))> =
            core::mem::take(&mut self.batch)
                .into_iter()
                .map(|(k, kv, r)| (k, (kv, r)))
                .collect();
        pairs.sort_by(|a, b| cmp_multi_key_in(&a.0, &b.0, self.descs, &[]));
        let mut run = factory()
            .map_err(|e| EngineError::Internal(alloc::format!("temp run create: {e:?}")))?;
        for (_, (key_values, row)) in pairs {
            let mut vals = key_values;
            vals.extend(row.values.iter().cloned());
            let row = Row::new(vals);
            let bytes = spg_storage::encode_row_body_dense(&row, &self.spill_schema);
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
            // Never spilled: one in-memory sort, exactly as before. The
            // key values were carried but never needed.
            self.batch
                .sort_by(|a, b| cmp_multi_key_in(&a.0, &b.0, self.descs, &[]));
            return Ok(self.batch.into_iter().map(|(_, _, r)| r).collect());
        }
        // Whatever is still held becomes the last run, so the merge has
        // one kind of input rather than two.
        self.spill_batch()?;

        let mut readers: Vec<RunReader> = Vec::with_capacity(self.runs.len());
        for mut run in core::mem::take(&mut self.runs) {
            let head = Self::next_row(&mut *run, &self.spill_schema, self.n_keys, &keys_of)?;
            readers.push(RunReader {
                run,
                head,
                n_keys: self.n_keys,
            });
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
            let n_keys = readers[i].n_keys;
            readers[i].head =
                Self::next_row(&mut *readers[i].run, &self.spill_schema, n_keys, &keys_of)?;
        }
        Ok(out)
    }

    /// `keys_of` receives a `Row` of just the KEY values — not the
    /// output row — so it can pack them with the same comparator the
    /// scan used, whether or not those columns were projected.
    fn next_row<K>(
        run: &mut dyn TempRun,
        schema: &TableSchema,
        n_keys: usize,
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
        let mut vals = row.values;
        let out = Row::new(vals.split_off(n_keys));
        let keys = keys_of(&Row::new(vals))?;
        Ok(Some((keys, out)))
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

    fn key_cols() -> Vec<ColumnSchema> {
        alloc::vec![ColumnSchema::new("k", DataType::Int, false)]
    }

    /// Pack a key value the way the scan's comparator does. Standing in
    /// for `build_order_keys_bound`, which needs an evaluation context.
    fn keys_of(key_row: &Row<'static>) -> Result<Vec<OrderKey>, EngineError> {
        match &key_row.values[0] {
            Value::Int(n) => Ok(alloc::vec![OrderKey::Int(i128::from(*n))]),
            other => Err(EngineError::Internal(alloc::format!("bad key: {other:?}"))),
        }
    }

    fn ids_of(rows: &[Row<'static>], at: usize) -> Vec<i32> {
        rows.iter()
            .map(|r| match r.values[at] {
                Value::Int(n) => n,
                _ => panic!("int column at {at}"),
            })
            .collect()
    }

    /// The case that matters and that the first version of these tests
    /// missed: the ORDER BY key is NOT in the projection. `SELECT pad
    /// FROM big ORDER BY id` is exactly this shape, and a run that
    /// stored only the projected row could not be re-keyed at all.
    #[test]
    fn a_key_that_was_never_projected_still_orders_the_merge() {
        let out_cols = alloc::vec![ColumnSchema::new("pad", DataType::Text, false)];
        let descs = [false];
        let mut s = ExternalSorter::new(Some(mem_run), 1, key_cols(), out_cols, &descs);
        for id in [7, 3, 9, 1, 8, 2, 6, 4, 5, 0] {
            let key_values = alloc::vec![Value::Int(id)];
            let keys = keys_of(&Row::new(key_values.clone())).unwrap();
            // The projected row carries the payload ONLY — no id.
            let row = Row::new(alloc::vec![Value::text(alloc::format!("pad-{id}"))]);
            s.push(keys, key_values, row).unwrap();
        }
        assert!(s.spilled(), "a 1-byte budget must have produced runs");
        let out = s.finish(keys_of).unwrap();
        let pads: Vec<alloc::string::String> = out
            .iter()
            .map(|r| match &r.values[0] {
                Value::Text(t) => t.to_string(),
                other => panic!("pad column, got {other:?}"),
            })
            .collect();
        assert_eq!(
            pads,
            alloc::vec![
                "pad-0", "pad-1", "pad-2", "pad-3", "pad-4", "pad-5", "pad-6", "pad-7", "pad-8",
                "pad-9"
            ],
            "ordered by the unprojected key, and the output row carries only the projection"
        );
        assert_eq!(out[0].values.len(), 1, "the key column must not leak into the result");
    }

    #[test]
    fn a_spilled_sort_returns_exactly_what_an_in_memory_sort_would() {
        let out_cols = alloc::vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new("pad", DataType::Text, false),
        ];
        let descs = [false];
        let mut s = ExternalSorter::new(Some(mem_run), 1, key_cols(), out_cols, &descs);
        for id in [7, 3, 9, 1, 8, 2, 6, 4, 5, 0] {
            let key_values = alloc::vec![Value::Int(id)];
            let keys = keys_of(&Row::new(key_values.clone())).unwrap();
            let row = Row::new(alloc::vec![
                Value::Int(id),
                Value::text(alloc::format!("pad-{id}")),
            ]);
            s.push(keys, key_values, row).unwrap();
        }
        assert!(s.spilled(), "a 1-byte budget must have produced runs");
        let out = s.finish(keys_of).unwrap();
        assert_eq!(ids_of(&out, 0), alloc::vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(
            out[0].values[1],
            Value::text("pad-0".to_string()),
            "the whole row comes back, not only its sort key"
        );
    }

    #[test]
    fn descending_order_survives_the_merge() {
        let out_cols = alloc::vec![ColumnSchema::new("id", DataType::Int, false)];
        let descs = [true];
        let mut s = ExternalSorter::new(Some(mem_run), 1, key_cols(), out_cols, &descs);
        for id in [3, 1, 2] {
            let key_values = alloc::vec![Value::Int(id)];
            let keys = keys_of(&Row::new(key_values.clone())).unwrap();
            s.push(keys, key_values, Row::new(alloc::vec![Value::Int(id)]))
                .unwrap();
        }
        assert!(s.spilled());
        assert_eq!(
            ids_of(&s.finish(keys_of).unwrap(), 0),
            alloc::vec![3, 2, 1],
            "DESC must survive the k-way merge"
        );
    }

    /// No factory: the old behaviour, byte for byte. A host with nowhere
    /// to spill must not start failing sorts it used to answer.
    #[test]
    fn without_a_factory_the_sort_stays_in_memory_and_still_sorts() {
        let out_cols = alloc::vec![ColumnSchema::new("id", DataType::Int, false)];
        let descs = [false];
        let mut s = ExternalSorter::new(None, 1, key_cols(), out_cols, &descs);
        for id in [2, 0, 1] {
            let key_values = alloc::vec![Value::Int(id)];
            let keys = keys_of(&Row::new(key_values.clone())).unwrap();
            s.push(keys, key_values, Row::new(alloc::vec![Value::Int(id)]))
                .unwrap();
        }
        assert!(!s.spilled(), "nothing to spill to means nothing spilled");
        assert_eq!(ids_of(&s.finish(keys_of).unwrap(), 0), alloc::vec![0, 1, 2]);
    }
}
