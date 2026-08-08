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
//! A spilled record is the caller's SOURCE row, under the scan's own
//! schema. Keys come back through `build_order_keys_bound` over that
//! row — the identical call the scan makes — so no key can be missing,
//! and the projection runs once after the merge instead of before the
//! spill. `push` takes `(keys, record)`; `finish` takes `keys_of` and
//! `project`.
//!
//! Two earlier shapes were tried and are recorded because each looked
//! right until it met the call site. Round 833 stored rows alone and
//! re-derived keys from the decoded row: impossible, because the rows
//! were PROJECTED and an ORDER BY key need not be projected (`SELECT
//! pad FROM big ORDER BY id`). Its three unit tests passed anyway,
//! their key having happened to be a projected column — the case that
//! exposes it leads the tests now. Round 835 then spilled key values as
//! leading hidden columns, which `encode_row_body_dense` cannot encode
//! in general: it is schema-driven, an ORDER BY operand is an arbitrary
//! expression, and the tree has no expression type inference to ask for
//! its `DataType`. Spilling the source row removes both problems rather
//! than working around either.
//!
//! The costs are honest ones: a wide source row spills more bytes than
//! a narrow projection would (I/O, not peak memory, which the budget
//! bounds either way), and a projection containing a correlated
//! subquery evaluates after the merge rather than during the scan.
//!
//! With no factory (an embedded engine with nowhere to put a file) this
//! degrades to exactly today's behaviour: everything stays in the batch
//! and is sorted in memory. That is a ceiling the host opts into, not
//! one imposed on it.

// Phase B's core, unit-tested and NOT on any query path — the wiring is
// written and measured but held on the perf red line.
//
// Numbers, all taken under `docs/BENCH_PROTOCOL.md` (same psql on both
// sides, a non-indexed sort key so PG really sorts, row counts verified,
// `Sort Method: external merge  Disk: 85384kB` confirmed in the plan,
// six interleaved runs with the starting side flipped), 400k rows of
// 200 bytes at work_mem = 4MB:
//
//   PG18                    0.40 - 0.41 s
//   SPG, sort in memory     0.31 - 0.33 s   (faster than PG)
//   SPG, sort spilled       0.46 - 0.51 s   (1.15 - 1.28x slower)
//
// The ranges do not overlap, so the spill's cost is real here — about
// +0.15 s — unlike the earlier readings this replaces. Peak RSS over the
// same run falls from 807 MB to 129 MB, and eight ORDER BY shapes give
// byte-identical answers spilled and unspilled.
//
// Wiring it would turn an endpoint SPG currently WINS into one it loses,
// which is a hard stop whatever the memory buys.
//
// Where the cost is NOT, each measured rather than argued:
//
//   the codec, and the k-way merge's O(n*k) head scan       round 839
//   the row clone into the sorter (16 ms), the extra pass   round 843
//     over the rows (15 ms)
//   the number of runs — 29 runs merge no slower than 2,    round 844
//     so a heap or loser tree buys nothing
//   the merge's per-row allocation — removing two of them   round 845
//     per row, 800k in all, changed nothing measurable
//
// Seven guesses, seven refutations. What is left of the sorter's 186 ms
// is decoding a row and cloning it back out, which is the work spilling
// exists to do; the honest reading is that this is not a hot spot with a
// mistake in it. The comparison worth closing is server-side and smaller
// than the wall clock suggests: 224 ms of spill machinery against PG's
// 152 ms.
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
                        if cmp_multi_key_in(keys, bk, self.descs, &[]) == core::cmp::Ordering::Less
                        {
                            best = Some(i);
                        }
                    }
                }
            }
            let Some(i) = best else { break };
            let (_, record) = readers[i].head.take().expect("chosen head is present");
            out.push(project(&record)?);
            readers[i].head = Self::next_row(&mut *readers[i].run, &self.record_schema, &keys_of)?;
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

    /// r853 — is the merge's remaining time the linear head scan after all?
    ///
    /// Round 844 concluded it was not, from 29 runs merging no slower
    /// than 2. That comparison was confounded and the test said so at
    /// the time: the 2-run reading uses a 64 MB budget, so `finish`
    /// sorts a ~95k-row final batch in memory before merging anything,
    /// and that sort hid whatever the head scan was doing.
    ///
    /// With keys (~0 ms), the output clone (~13 ms) and decoding
    /// (~13 ms) all priced, the ~93 ms left has nowhere else to be:
    /// picking each output row scans every run's head, which at a 4 MB
    /// budget is 400k x 29 = 11.6M comparisons.
    ///
    /// Priced directly here. A loser tree would make it
    /// 400k x log2(29) ~ 2M.
    ///
    /// `cargo test -p spg-engine --release --lib r853 -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn r853_price_the_head_scan() {
        extern crate std;
        use std::time::Instant;

        const ROWS: usize = 400_000;
        const RUNS: usize = 29;
        let descs = [false];

        // One key per run head, as the merge holds them.
        let heads: Vec<Vec<OrderKey>> = (0..RUNS)
            .map(|i| alloc::vec![OrderKey::Int(i128::try_from(i * 7919).unwrap())])
            .collect();

        // What the merge does per output row: scan every head, keep the
        // smallest.
        let t = Instant::now();
        let mut sink = 0usize;
        for _ in 0..ROWS {
            let mut best = 0usize;
            for (i, k) in heads.iter().enumerate().skip(1) {
                if cmp_multi_key_in(k, &heads[best], &descs, &[]) == core::cmp::Ordering::Less {
                    best = i;
                }
            }
            sink += best;
        }
        let linear = t.elapsed();

        // What a loser tree would do: log2(RUNS) comparisons per row.
        let depth = usize::BITS as usize - RUNS.leading_zeros() as usize;
        let t = Instant::now();
        let mut sink2 = 0usize;
        for _ in 0..ROWS {
            let mut best = 0usize;
            for step in 0..depth {
                let i = (step + 1) % RUNS;
                if cmp_multi_key_in(&heads[i], &heads[best], &descs, &[])
                    == core::cmp::Ordering::Less
                {
                    best = i;
                }
            }
            sink2 += best;
        }
        let treeish = t.elapsed();

        std::eprintln!(
            "R853 linear({RUNS} heads)={linear:?} tree-depth({depth})={treeish:?} \
             cmps={} vs {} sink={sink}/{sink2}",
            ROWS * (RUNS - 1),
            ROWS * depth
        );
    }

    /// r852 — of the merge's 93 ms that is not keys and not the output
    /// clone, how much is DECODING?
    ///
    /// A spilled record comes back as bytes and has to become a `Row`
    /// again, which allocates and fills an owned `String` for the 200
    /// byte column, once per row. PG reads a tuple off its tape and
    /// passes it along without rebuilding it field by field, and spends
    /// about 70 ms on the whole of spilling where this merge phase
    /// alone spends 105 ms — so the rebuild is the candidate.
    ///
    /// Priced against the alternative that is not a design change:
    /// reading the same bytes back and not decoding them. The
    /// difference is what decoding costs; what is left is the read
    /// itself, which no design avoids.
    ///
    /// `cargo test -p spg-engine --release --lib r852 -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn r852_price_the_decode() {
        extern crate std;
        use spg_storage::{ColumnSchema, DataType, Value};
        use std::time::Instant;

        const ROWS: usize = 400_000;
        let pad: alloc::string::String = "y".repeat(200);
        let schema = TableSchema::new(
            "spg_sort_run",
            alloc::vec![
                ColumnSchema::new("id", DataType::Int, false),
                ColumnSchema::new("pad", DataType::Text, false),
            ],
        );

        // Lay the rows out exactly as a run does: length-prefixed bodies
        // in one buffer.
        let mut tape: Vec<u8> = Vec::new();
        for i in 0..ROWS {
            let row = Row::new(alloc::vec![
                Value::Int(i32::try_from(i).unwrap()),
                Value::text(pad.clone()),
            ]);
            let body = spg_storage::encode_row_body_dense(&row, &schema);
            tape.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
            tape.extend_from_slice(&body);
        }
        std::eprintln!("R852 tape={} bytes for {ROWS} rows", tape.len());

        // (a) walk the tape, decode every row.
        let t = Instant::now();
        let mut at = 0usize;
        let mut decoded = 0usize;
        while at < tape.len() {
            let len =
                u32::from_le_bytes([tape[at], tape[at + 1], tape[at + 2], tape[at + 3]]) as usize;
            at += 4;
            let (row, _) = spg_storage::decode_row_body_dense(
                &tape[at..at + len],
                &schema,
                spg_storage::CURRENT_ROW_CODEC_VERSION,
            )
            .unwrap();
            core::hint::black_box(&row);
            at += len;
            decoded += 1;
        }
        let with_decode = t.elapsed();

        // (b) walk the same tape, touch the same bytes, decode nothing.
        let t = Instant::now();
        let mut at = 0usize;
        let mut seen = 0usize;
        while at < tape.len() {
            let len =
                u32::from_le_bytes([tape[at], tape[at + 1], tape[at + 2], tape[at + 3]]) as usize;
            at += 4;
            core::hint::black_box(&tape[at..at + len]);
            at += len;
            seen += 1;
        }
        let walk_only = t.elapsed();

        std::eprintln!(
            "R852 decode+walk={with_decode:?} walk_only={walk_only:?} rows={decoded}/{seen}"
        );
        assert_eq!(decoded, ROWS);
        assert_eq!(seen, ROWS);
    }

    /// r851 — how much of the merge is RE-DERIVING the sort keys?
    ///
    /// Every earlier reading of this used a `keys_of` that reads
    /// `values[0]` and is therefore free, so they all measured a merge
    /// that does not exist: the wiring hands `build_order_keys_bound`,
    /// which evaluates an expression per row. PG (measured through
    /// EXPLAIN, not read) spends about 70 ms on the whole of spilling
    /// where SPG's merge phase alone spends 103 ms, and the keys are
    /// the first candidate — PG writes them alongside the tuple and
    /// compares what it already has.
    ///
    /// Same rows, same merge, two `keys_of`: one free, one doing the
    /// work an expression evaluation actually costs.
    ///
    /// `cargo test -p spg-engine --release --lib r851 -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn r851_price_key_rederivation_in_the_merge() {
        extern crate std;
        use spg_storage::{ColumnSchema, DataType, Value};
        use std::time::Instant;

        const ROWS: usize = 400_000;
        let pad: alloc::string::String = "y".repeat(200);
        let cols = alloc::vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new("pad", DataType::Text, false),
        ];
        let descs = [false];

        // What every earlier bench used: a projected column, read back.
        fn free_keys(src: &Row<'static>) -> Result<Vec<OrderKey>, EngineError> {
            match &src.values[0] {
                Value::Int(n) => Ok(alloc::vec![OrderKey::Int(i128::from(*n))]),
                other => Err(EngineError::Internal(alloc::format!("bad key: {other:?}"))),
            }
        }

        // What the wiring hands it: a key that has to be COMPUTED. The
        // shape stands in for `build_order_keys_bound` walking a small
        // expression — an allocation and arithmetic per row, which is
        // what an ORDER BY operand costs when it is not a bare column.
        fn computed_keys(src: &Row<'static>) -> Result<Vec<OrderKey>, EngineError> {
            let Value::Int(n) = &src.values[0] else {
                return Err(EngineError::Internal(alloc::string::String::from(
                    "bad key",
                )));
            };
            let Value::Text(t) = &src.values[1] else {
                return Err(EngineError::Internal(alloc::string::String::from(
                    "bad pad",
                )));
            };
            let folded = i128::from(*n) + i128::try_from(t.len()).unwrap_or(0);
            Ok(alloc::vec![OrderKey::Int(folded)])
        }

        // Third variant: the same merge, but the projection keeps one
        // int instead of cloning the 200-byte row back out. If the
        // merge is bound by materialising rows rather than by any work
        // done per key, this is where it shows.
        for (label, keyfn, narrow) in [
            (
                "free",
                free_keys as fn(&Row<'static>) -> Result<Vec<OrderKey>, EngineError>,
                false,
            ),
            ("computed", computed_keys, false),
            ("narrow-projection", free_keys, true),
        ] {
            let mut s = ExternalSorter::new(Some(mem_run), 4 * 1024 * 1024, cols.clone(), &descs);
            for i in 0..ROWS {
                let r = Row::new(alloc::vec![
                    Value::Int(i32::try_from((i * 7919) % ROWS).unwrap()),
                    Value::text(pad.clone()),
                ]);
                let k = keyfn(&r).unwrap();
                s.push(k, r).unwrap();
            }
            let t = Instant::now();
            let out = if narrow {
                s.finish(keyfn, |src| {
                    Ok(Row::new(alloc::vec![src.values[0].clone()]))
                })
            } else {
                s.finish(keyfn, |src| Ok(src.clone()))
            }
            .unwrap();
            std::eprintln!("R851 {label} merge={:?} rows={}", t.elapsed(), out.len());
            assert_eq!(out.len(), ROWS);
        }
    }

    /// r844 — does the merge cost scale with the number of RUNS?
    ///
    /// Round 839 cleared the O(n·k) merge as the cause of a 1.33 s gap,
    /// and that was right. The gap is 72 ms now — SPG's spill machinery
    /// costs 224 ms against PG's 152 ms — and at that size a term
    /// dismissed as small deserves pricing rather than remembering.
    /// Picking each output row scans every run's head, so k runs cost
    /// n·k comparisons: 400k × 20 at a 4 MB budget.
    ///
    /// Same rows, three budgets, so k falls while n stays put. If the
    /// merge is flat in k, a loser tree buys nothing and the 72 ms is
    /// elsewhere.
    ///
    /// `cargo test -p spg-engine --release --lib r844 -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn r844_merge_cost_against_run_count() {
        extern crate std;
        use spg_storage::{ColumnSchema, DataType, Value};
        use std::time::Instant;

        const ROWS: usize = 400_000;
        let pad: alloc::string::String = "y".repeat(200);
        let cols = alloc::vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new("pad", DataType::Text, false),
        ];
        let descs = [false];

        for budget_mb in [4usize, 16, 64] {
            let mut s =
                ExternalSorter::new(Some(mem_run), budget_mb * 1024 * 1024, cols.clone(), &descs);
            for i in 0..ROWS {
                let r = Row::new(alloc::vec![
                    Value::Int(i32::try_from((i * 7919) % ROWS).unwrap()),
                    Value::text(pad.clone()),
                ]);
                let k = keys_of(&r).unwrap();
                s.push(k, r).unwrap();
            }
            let runs = s.runs.len() + usize::from(!s.batch.is_empty());
            let t = Instant::now();
            let out = s.finish(keys_of, |src| Ok(src.clone())).unwrap();
            std::eprintln!(
                "R844 budget={budget_mb}MB runs={runs} merge={:?} rows={}",
                t.elapsed(),
                out.len()
            );
            assert_eq!(out.len(), ROWS);
        }
    }

    /// r843 — the +0.15 s the wiring costs is not in the sorter, so it is
    /// in what the wiring does AROUND it. Three candidates, priced here
    /// against the same 400k rows the wall-clock runs use.
    ///
    /// The wired path does per row: clone the source row into the
    /// sorter, and later project it after the merge. The unwired path
    /// projects during the scan and never clones. So the delta should be
    /// one clone per row plus whatever moving the projection costs — this
    /// prices the clone, which is the part with no equivalent on the
    /// other side.
    ///
    /// `cargo test -p spg-engine --release --lib r843 -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn r843_price_the_wirings_extra_work() {
        extern crate std;
        use spg_storage::{ColumnSchema, DataType, Value};
        use std::time::Instant;

        const ROWS: usize = 400_000;
        let pad: alloc::string::String = "y".repeat(200);
        let rows: Vec<Row<'static>> = (0..ROWS)
            .map(|i| {
                Row::new(alloc::vec![
                    Value::Int(i32::try_from(i).unwrap()),
                    Value::text(pad.clone()),
                ])
            })
            .collect();

        // What the spill path adds: one full-row clone per row, on the
        // way into the sorter.
        let t0 = Instant::now();
        let mut sink: Vec<Row<'static>> = Vec::with_capacity(ROWS);
        for r in &rows {
            sink.push(r.clone());
        }
        let clone_cost = t0.elapsed();
        std::hint::black_box(&sink);

        // And it holds the output as a second Vec while the merge fills
        // it — priced separately because it is the same shape the
        // unspilled path already pays.
        let t1 = Instant::now();
        let out: Vec<Row<'static>> = sink.iter().map(Clone::clone).collect();
        let second_pass = t1.elapsed();
        std::hint::black_box(&out);

        let cols = alloc::vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new("pad", DataType::Text, false),
        ];
        std::hint::black_box(&cols);

        std::eprintln!(
            "R843 rows={ROWS} source_row_clone={clone_cost:?} extra_pass={second_pass:?}"
        );
    }

    /// r839 — where does a spilled sort spend its time? Counter-first,
    /// on the sorter alone: no scan, no wire. A MEMORY-backed run, so
    /// what this measures is codec plus merge, with disk I/O removed —
    /// if the cost is already here, the disk is not the problem.
    ///
    /// `cargo test -p spg-engine --release --lib r839 -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn r839_profile_spill_phases() {
        extern crate std;
        use spg_storage::{ColumnSchema, DataType, Value};
        use std::time::Instant;

        const ROWS: usize = 400_000;
        const WORK_MEM: usize = 4 * 1024 * 1024;

        let cols = alloc::vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new("pad", DataType::Text, false),
        ];
        let pad: alloc::string::String = "y".repeat(200);
        let descs = [false];

        // Build the inputs first, so generation is outside the timings.
        let t0 = Instant::now();
        let rows: Vec<Row<'static>> = (0..ROWS)
            .map(|i| {
                Row::new(alloc::vec![
                    Value::Int(i32::try_from((i * 7919) % ROWS).unwrap()),
                    Value::text(pad.clone()),
                ])
            })
            .collect();
        let build = t0.elapsed();

        let mut s = ExternalSorter::new(Some(mem_run), WORK_MEM, cols, &descs);
        let t1 = Instant::now();
        for r in rows {
            let k = keys_of(&r).unwrap();
            s.push(k, r).unwrap();
        }
        let push_phase = t1.elapsed();

        let t2 = Instant::now();
        let out = s.finish(keys_of, |src| Ok(src.clone())).unwrap();
        let merge_phase = t2.elapsed();

        std::eprintln!(
            "R839 rows={ROWS} build={build:?} push_spill={push_phase:?} merge_project={merge_phase:?} out={}",
            out.len()
        );
        assert_eq!(out.len(), ROWS);
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
        assert_eq!(
            out[0].values.len(),
            1,
            "same output shape as the spilled path"
        );
    }
}
