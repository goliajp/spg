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
//! A spilled record is `key_values ++ projected_values` under a schema
//! built the same way; on decode the leading values split off, get
//! packed into keys through the caller's own packer, and the remainder
//! is the output row. Values are what the row codec already encodes, so
//! there is no second format to keep in step with it.
//!
//! Round 833 first tried storing rows ALONE and re-deriving keys from
//! the decoded row. That cannot work: the rows a sort spills are
//! PROJECTED, and an ORDER BY key need not be projected — `SELECT pad
//! FROM big ORDER BY id` sorts on something the row no longer carries.
//! Its three unit tests passed regardless, because their key happened
//! to be a projected column; the case that exposes it leads the tests
//! now.
//!
//! ⚠️ **Wiring found a second obstacle, and it points at a simpler
//! design than this one** (round 836; recorded here rather than left to
//! be rediscovered).
//!
//! `encode_row_body_dense` is schema-driven: every column needs a
//! declared `DataType`. The projected columns have one, but an ORDER BY
//! operand is an arbitrary expression, and the tree has no expression
//! type inference to ask — so the key columns of the schema above
//! cannot be built for the general case.
//!
//! Spilling the SOURCE row instead removes the problem rather than
//! working around it. Its schema is the scan's own `schema_cols`, types
//! and all; keys come back through `build_order_keys_bound` over that
//! row — the identical call the scan makes, so no key can be missing by
//! construction; and the projection runs once, after the merge, instead
//! of before the spill. No key columns, no type inference, no hidden
//! values. The costs are honest ones: a wide source row spills more
//! bytes than a narrow projection would (I/O, not peak memory, which
//! stays bounded by the budget either way), and a projection containing
//! a correlated subquery evaluates after the merge rather than during
//! the scan.
//!
//! That would make `push` take `(keys, record)` and `finish` take a
//! `project` alongside `keys_of`, and the key-values plumbing below
//! becomes unnecessary. Left as it stands until the wiring lands, so the
//! change arrives with the call site that proves it.
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
    /// The record at this run's head, and its keys. `None` once drained.
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
    /// The schema of the RECORD a run stores — the caller's source row,
    /// whose types are already declared.
    record_schema: TableSchema,
    descs: &'a [bool],
    batch: Vec<(Vec<OrderKey>, Row<'static>)>,
    batch_bytes: usize,
    runs: Vec<Box<dyn TempRun>>,
}

impl<'a> ExternalSorter<'a> {
    /// `record_cols` describes the rows handed to `push` — the scan's
    /// own source columns, not the projection. Sorting works on records
    /// that still carry every column the ORDER BY might name.
    pub(crate) fn new(
        factory: Option<crate::TempRunFactory>,
        budget_bytes: usize,
        record_cols: Vec<spg_storage::ColumnSchema>,
        descs: &'a [bool],
    ) -> Self {
        Self {
            factory,
            budget_bytes,
            record_schema: TableSchema::new("spg_sort_run", record_cols),
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

    /// `record` is the row the caller wants back in order — the source
    /// row, so a spilled record can always be re-keyed.
    pub(crate) fn push(
        &mut self,
        keys: Vec<OrderKey>,
        record: Row<'static>,
    ) -> Result<(), EngineError> {
        self.batch_bytes += crate::bytebudget::approx_values_bytes(&record.values);
        self.batch.push((keys, record));
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
            let bytes = spg_storage::encode_row_body_dense(&row, &self.record_schema);
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
    /// Every row, in order, projected.
    ///
    /// `keys_of` re-derives a spilled record's keys — the caller passes
    /// the same call the scan used, over the same source row, so no key
    /// can be missing. `project` turns a record into the output row, and
    /// runs once per row whether or not the sort spilled.
    pub(crate) fn finish<K, P>(
        mut self,
        keys_of: K,
        project: P,
    ) -> Result<Vec<Row<'static>>, EngineError>
    where
        K: Fn(&Row<'static>) -> Result<Vec<OrderKey>, EngineError>,
        P: Fn(&Row<'static>) -> Result<Row<'static>, EngineError>,
    {
        if self.runs.is_empty() {
            // Never spilled: one in-memory sort, exactly as before.
            crate::orderby::sort_by_keys(&mut self.batch, self.descs);
            return self
                .batch
                .into_iter()
                .map(|(_, r)| project(&r))
                .collect::<Result<Vec<_>, _>>();
        }
        // Whatever is still held becomes the last run, so the merge has
        // one kind of input rather than two.
        self.spill_batch()?;

        let mut readers: Vec<RunReader> = Vec::with_capacity(self.runs.len());
        for mut run in core::mem::take(&mut self.runs) {
            let head = Self::next_row(&mut *run, &self.record_schema, &keys_of)?;
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
            let (_, record) = readers[i].head.take().expect("chosen head is present");
            out.push(project(&record)?);
            readers[i].head =
                Self::next_row(&mut *readers[i].run, &self.record_schema, &keys_of)?;
        }
        Ok(out)
    }

    /// `keys_of` receives the decoded SOURCE row, which still carries
    /// every column an ORDER BY could name.
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

    /// The scan's own columns: a spilled record keeps them ALL, so the
    /// ORDER BY can name one the projection drops.
    fn record_cols() -> Vec<ColumnSchema> {
        alloc::vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new("pad", DataType::Text, false),
        ]
    }

    fn record(id: i32) -> Row<'static> {
        Row::new(alloc::vec![
            Value::Int(id),
            Value::text(alloc::format!("pad-{id}")),
        ])
    }

    /// Pack the key from the SOURCE row, standing in for
    /// `build_order_keys_bound` (which needs an evaluation context).
    fn keys_of(src: &Row<'static>) -> Result<Vec<OrderKey>, EngineError> {
        match &src.values[0] {
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

    /// Project away the key column, so the output carries `pad` alone.
    fn project_pad_only(src: &Row<'static>) -> Result<Row<'static>, EngineError> {
        Ok(Row::new(alloc::vec![src.values[1].clone()]))
    }

    fn project_identity(src: &Row<'static>) -> Result<Row<'static>, EngineError> {
        Ok(src.clone())
    }

    /// The case the first two designs got wrong: the ORDER BY key is not
    /// in the projection. `SELECT pad FROM big ORDER BY id` is this
    /// shape. Spilling the SOURCE row makes it work by construction —
    /// the key is still there to be read.
    #[test]
    fn a_key_that_was_never_projected_still_orders_the_merge() {
        let descs = [false];
        let mut s = ExternalSorter::new(Some(mem_run), 1, record_cols(), &descs);
        for id in [7, 3, 9, 1, 8, 2, 6, 4, 5, 0] {
            let r = record(id);
            let k = keys_of(&r).unwrap();
            s.push(k, r).unwrap();
        }
        assert!(s.spilled(), "a 1-byte budget must have produced runs");
        let out = s.finish(keys_of, project_pad_only).unwrap();
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
            "ordered by the unprojected key"
        );
        assert_eq!(
            out[0].values.len(),
            1,
            "the key column must not leak into the result"
        );
    }

    #[test]
    fn a_spilled_sort_returns_exactly_what_an_in_memory_sort_would() {
        let descs = [false];
        let mut s = ExternalSorter::new(Some(mem_run), 1, record_cols(), &descs);
        for id in [7, 3, 9, 1, 8, 2, 6, 4, 5, 0] {
            let r = record(id);
            let k = keys_of(&r).unwrap();
            s.push(k, r).unwrap();
        }
        assert!(s.spilled());
        let out = s.finish(keys_of, project_identity).unwrap();
        assert_eq!(ids_of(&out, 0), alloc::vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(
            out[0].values[1],
            Value::text("pad-0".to_string()),
            "the whole row comes back, not only its sort key"
        );
    }

    #[test]
    fn descending_order_survives_the_merge() {
        let descs = [true];
        let mut s = ExternalSorter::new(Some(mem_run), 1, record_cols(), &descs);
        for id in [3, 1, 2] {
            let r = record(id);
            let k = keys_of(&r).unwrap();
            s.push(k, r).unwrap();
        }
        assert!(s.spilled());
        assert_eq!(
            ids_of(&s.finish(keys_of, project_identity).unwrap(), 0),
            alloc::vec![3, 2, 1],
            "DESC must survive the k-way merge"
        );
    }

    /// No factory: the old behaviour, byte for byte. A host with nowhere
    /// to spill must not start failing sorts it used to answer — and the
    /// projection still runs, so the result shape does not depend on
    /// whether a spill happened.
    #[test]
    fn without_a_factory_the_sort_stays_in_memory_and_still_sorts() {
        let descs = [false];
        let mut s = ExternalSorter::new(None, 1, record_cols(), &descs);
        for id in [2, 0, 1] {
            let r = record(id);
            let k = keys_of(&r).unwrap();
            s.push(k, r).unwrap();
        }
        assert!(!s.spilled(), "nothing to spill to means nothing spilled");
        let out = s.finish(keys_of, project_pad_only).unwrap();
        let pads: Vec<alloc::string::String> = out
            .iter()
            .map(|r| match &r.values[0] {
                Value::Text(t) => t.to_string(),
                other => panic!("pad column, got {other:?}"),
            })
            .collect();
        assert_eq!(pads, alloc::vec!["pad-0", "pad-1", "pad-2"]);
        assert_eq!(out[0].values.len(), 1, "same output shape as the spilled path");
    }
}
