//! Sorting a TOTAL order across threads.
//!
//! v7.40.4 — the sort panel's remaining losses were not a comparator
//! defect. Measured on the panel's own fixtures at 400,000 rows, with
//! `EXPLAIN (ANALYZE, TIMING OFF)` on both legs and the panel's own
//! `work_mem`:
//!
//! | cell                         |   SPG | PG par | PG serial | workers |
//! |------------------------------|------:|-------:|----------:|--------:|
//! | int                          |  39.7 |   59.3 |      59.1 |       0 |
//! | two keys                     |  54.3 |   68.8 |      68.6 |       0 |
//! | text (26 values)             |  78.3 |   82.0 |     111.3 |       2 |
//! | short text distinct          |  55.1 |   78.3 |      76.5 |       0 |
//! | long text distinct           |  80.0 |   63.7 |     122.1 |       2 |
//! | long text, key not projected | 110.7 |  102.4 |     177.3 |       2 |
//! | long text top-N              |   9.7 |    9.9 |      18.2 |       2 |
//!
//! Seven cells out of seven: every cell SPG lost or tied is a cell where
//! PostgreSQL launched workers, and every cell PostgreSQL ran on one
//! process SPG won by 0.67x-0.79x. Per core SPG was already ahead
//! everywhere. What was left on the scoreboard was two CPUs, and no
//! amount of work on the comparator reaches it.
//!
//! # Why the result is bit-identical to the serial sort
//!
//! Every comparator the prefix sort uses ends in `.then_with(|| ia.cmp(&ib))`
//! on the row index, so no two distinct elements ever compare `Equal`: the
//! order is STRICT and TOTAL. A total order has exactly one sorted
//! arrangement, so splitting, sorting the pieces and merging them cannot
//! produce a different answer than sorting the whole — there is no other
//! answer to produce. That is what makes this safe to do underneath a
//! sort whose stability the rest of the file depends on, and it is what
//! `e2e_sort_parallel_v7404` pins by asking for the same query with the
//! workers on and off and comparing the rows.
//!
//! # What decides how many threads
//!
//! `max_parallel_workers_per_gather`, which SPG has always accepted and
//! never read. PG's own default is 2 — two extra workers plus the one
//! doing the asking, three sorting processes — so a customer who has
//! tuned that GUC for their PostgreSQL gets the same shape here, and a
//! customer who sets it to 0 gets the serial path exactly as before.

#[cfg(feature = "std")]
extern crate std;

use alloc::vec::Vec;
use core::cmp::Ordering;

/// Under this many elements the split is not worth its own scheduling.
/// A 64k-element sort is about 1 ms serially; spawning for that is
/// spending more than it can save.
const MIN_PARALLEL: usize = 65_536;

/// No chunk smaller than this, so a modest input does not fan out into
/// threads that each finish before the last one starts.
const MIN_CHUNK: usize = 16_384;

/// The largest fan-out for ONE sort, whatever the GUC says.
const MAX_PARTS: usize = 8;

/// Extra threads currently leased by sorts anywhere in this process.
///
/// `max_parallel_workers_per_gather` bounds one query; PostgreSQL bounds
/// the whole cluster with `max_parallel_workers`, and without the second
/// one a server does not have a limit at all -- twenty connections each
/// sorting would take twenty times the per-query fan-out and spend the
/// difference on context switches. That GUC is in SPG's catalogue too,
/// with PG's own default of 8, and was read by nobody.
static LEASED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Threads taken for the duration of one sort, returned however it ends.
struct Lease(usize);

impl Drop for Lease {
    fn drop(&mut self) {
        LEASED.fetch_sub(self.0, core::sync::atomic::Ordering::Relaxed);
    }
}

/// What a sort may take given what is already out: as much of `want` as
/// `cap` still has room for, and none when it has none — a sort that
/// arrives while the machine is busy runs serially rather than queueing,
/// because it has useful work to do either way.
///
/// Separate from the counter so it can be asked directly. A test that
/// reasons about the process-wide total cannot: the counter is shared
/// with every other test running at the same time, which is the whole
/// point of it.
fn lease_amount(held: usize, want: usize, cap: usize) -> usize {
    want.min(cap.saturating_sub(held))
}

/// Take up to `want` extra threads without pushing the process past
/// `cap`.
fn take_lease(want: usize, cap: usize) -> Lease {
    use core::sync::atomic::Ordering::Relaxed;
    let mut held = LEASED.load(Relaxed);
    loop {
        let take = lease_amount(held, want, cap);
        if take == 0 {
            return Lease(0);
        }
        match LEASED.compare_exchange_weak(held, held + take, Relaxed, Relaxed) {
            Ok(_) => return Lease(take),
            Err(now) => held = now,
        }
    }
}

/// How many pieces to split into.
///
/// `extra_workers + 1` rather than `extra_workers`, because PG's GUC
/// counts the workers a Gather adds to the process that already has the
/// query: at its default of 2 PostgreSQL sorts in three processes, and
/// this splits into three. The count is not rounded to a power of two —
/// `merge_round` carries an odd trailing run through, and matching the
/// other engine's concurrency is worth more than a balanced last merge.
fn parts_for(n: usize, extra_workers: usize) -> usize {
    if n < MIN_PARALLEL || extra_workers == 0 {
        return 1;
    }
    (extra_workers + 1).min(n / MIN_CHUNK).min(MAX_PARTS)
}

/// Sort under a comparator that is a strict total order.
///
/// Takes the vector by value and gives it back, because the two callers
/// carry different element types -- the prefix path's `(u64, u32)` is
/// `Copy` and the collation path's `(Vec<u8>, u32)` is not -- and one
/// body that MOVES elements serves both. A body written for `Copy`
/// would have left the collated sort, which is what a customer on a
/// locale collation actually runs, on one thread.
///
/// `extra_workers` is the GUC's value; 0 means the serial path, and so
/// does a build without `std`.
#[cfg(not(feature = "std"))]
pub(crate) fn sort_total<T, F>(mut v: Vec<T>, _how: Workers, cmp: &F) -> Vec<T>
where
    T: Send,
    F: Fn(&T, &T) -> Ordering + Sync,
{
    v.sort_unstable_by(|a, b| cmp(a, b));
    v
}

/// The same, for a comparator that CAN report `Equal` on two distinct
/// elements — where the answer therefore depends on the sort being
/// stable, as `sort_by` is.
///
/// A parallel merge sort is stable exactly when its pieces are and the
/// merge prefers the earlier run on a tie, which is what `merge_two`
/// does. The two entry points differ only in that: the strict-total-order
/// one sorts its pieces with the faster unstable sort, because with no
/// ties there is nothing for stability to preserve.
#[cfg(not(feature = "std"))]
pub(crate) fn sort_total_stable<T, F>(mut v: Vec<T>, _how: Workers, cmp: &F) -> Vec<T>
where
    T: Send,
    F: Fn(&T, &T) -> Ordering + Sync,
{
    v.sort_by(|a, b| cmp(a, b));
    v
}

/// What the two GUCs say: extra threads for one sort, and for the
/// process.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Workers {
    pub per_sort: usize,
    pub per_process: usize,
}

impl Workers {
    /// One thread. For the callers that have no session to ask — the
    /// wrappers the tests use, and any sort small enough that the
    /// question does not arise.
    pub(crate) const fn serial() -> Self {
        Self {
            per_sort: 0,
            per_process: 0,
        }
    }
}

#[cfg(feature = "std")]
pub(crate) fn sort_total<T, F>(v: Vec<T>, how: Workers, cmp: &F) -> Vec<T>
where
    T: Send,
    F: Fn(&T, &T) -> Ordering + Sync,
{
    sort_in(v, how, cmp, false)
}

/// The same, for a comparator that CAN report `Equal` on two distinct
/// elements — where the answer therefore depends on the sort being
/// stable, as `sort_by` is.
///
/// A parallel merge sort is stable exactly when its pieces are and the
/// merge prefers the earlier run on a tie, which is what `merge_two`
/// does. The two entry points differ only in that: the strict-total-order
/// one sorts its pieces with the faster unstable sort, because with no
/// ties there is nothing for stability to preserve.
#[cfg(feature = "std")]
pub(crate) fn sort_total_stable<T, F>(v: Vec<T>, how: Workers, cmp: &F) -> Vec<T>
where
    T: Send,
    F: Fn(&T, &T) -> Ordering + Sync,
{
    sort_in(v, how, cmp, true)
}

#[cfg(feature = "std")]
fn sort_in<T, F>(mut v: Vec<T>, how: Workers, cmp: &F, stable: bool) -> Vec<T>
where
    T: Send,
    F: Fn(&T, &T) -> Ordering + Sync,
{
    let n = v.len();
    let wanted = parts_for(n, how.per_sort);
    // The lease is held until this function returns, so the threads are
    // given back whether the sort finishes or unwinds.
    let lease = take_lease(wanted.saturating_sub(1), how.per_process);
    let parts = lease.0 + 1;
    if parts < 2 {
        if stable {
            v.sort_by(|a, b| cmp(a, b));
        } else {
            v.sort_unstable_by(|a, b| cmp(a, b));
        }
        return v;
    }
    let base = n / parts;
    let rem = n % parts;
    let mut runs: Vec<Vec<T>> = Vec::with_capacity(parts);
    let mut src = v.into_iter();
    for i in 0..parts {
        runs.push(src.by_ref().take(base + usize::from(i < rem)).collect());
    }
    std::thread::scope(|s| {
        let mut rest: &mut [Vec<T>] = runs.as_mut_slice();
        while let Some((head, tail)) = rest.split_first_mut() {
            rest = tail;
            s.spawn(move || {
                if stable {
                    head.sort_by(|a, b| cmp(a, b));
                } else {
                    head.sort_unstable_by(|a, b| cmp(a, b));
                }
            });
        }
    });
    while runs.len() > 1 {
        runs = merge_round(runs, cmp);
    }
    drop(lease);
    runs.pop().unwrap_or_default()
}

/// Merge adjacent pairs of runs, each pair on its own thread. An odd
/// trailing run is carried through untouched.
#[cfg(feature = "std")]
fn merge_round<T, F>(runs: Vec<Vec<T>>, cmp: &F) -> Vec<Vec<T>>
where
    T: Send,
    F: Fn(&T, &T) -> Ordering + Sync,
{
    let mut pairs: Vec<(Vec<T>, Option<Vec<T>>)> = Vec::with_capacity(runs.len().div_ceil(2));
    let mut it = runs.into_iter();
    while let Some(a) = it.next() {
        pairs.push((a, it.next()));
    }
    std::thread::scope(|s| {
        let handles: Vec<_> = pairs
            .into_iter()
            .map(|(a, b)| {
                s.spawn(move || match b {
                    None => a,
                    Some(b) => merge_two(a, b, cmp),
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("a merge thread panicked"))
            .collect()
    })
}

/// One sequential merge. Ties take from `a` first; under a strict total
/// order there are none, and the rule costs nothing to state.
#[cfg(feature = "std")]
fn merge_two<T, F>(a: Vec<T>, b: Vec<T>, cmp: &F) -> Vec<T>
where
    F: Fn(&T, &T) -> Ordering,
{
    enum Take {
        A,
        B,
        RestA,
        RestB,
    }
    let mut out = Vec::with_capacity(a.len() + b.len());
    let mut ai = a.into_iter().peekable();
    let mut bi = b.into_iter().peekable();
    loop {
        // The decision is taken BEFORE anything moves, so the peeks are
        // no longer borrowed when the arm below consumes an iterator.
        let take = match (ai.peek(), bi.peek()) {
            (Some(x), Some(y)) => {
                if cmp(x, y) == Ordering::Greater {
                    Take::B
                } else {
                    Take::A
                }
            }
            (Some(_), None) => Take::RestA,
            (None, Some(_)) => Take::RestB,
            (None, None) => return out,
        };
        match take {
            Take::A => out.extend(ai.next()),
            Take::B => out.extend(bi.next()),
            Take::RestA => {
                out.extend(ai);
                return out;
            }
            Take::RestB => {
                out.extend(bi);
                return out;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// Enough process-wide budget that the per-sort GUC is what decides.
    fn plenty(per_sort: usize) -> Workers {
        Workers {
            per_sort,
            per_process: 64,
        }
    }

    fn total(a: &(u64, u32), b: &(u64, u32)) -> Ordering {
        a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1))
    }

    /// The whole safety argument in one assertion: whatever the split,
    /// the answer is the serial answer.
    #[test]
    fn every_fan_out_gives_the_serial_answer() {
        for n in [0usize, 1, 2, 3, 65_535, 65_536, 70_001, 131_072] {
            let v: Vec<(u64, u32)> = (0..n as u32)
                .map(|i| ((u64::from(i) * 7919) % 1000, i))
                .collect();
            let mut want = v.clone();
            want.sort_unstable_by(total);
            for workers in [0usize, 1, 2, 3, 7, 64] {
                let got = sort_total(v.clone(), plenty(workers), &total);
                assert_eq!(got, want, "n={n} workers={workers}");
            }
        }
    }

    #[test]
    fn an_odd_run_count_carries_the_last_one_through() {
        // `merge_round` has to carry an odd trailing run through, and a
        // fan-out chosen by the GUC can ask for one.
        let runs = vec![
            vec![(1u64, 0u32), (3, 1)],
            vec![(2u64, 2u32), (4, 3)],
            vec![(0u64, 4u32)],
        ];
        let out = merge_round(runs, &total);
        assert_eq!(
            out,
            vec![vec![(1, 0), (2, 2), (3, 1), (4, 3)], vec![(0, 4)]]
        );
    }

    /// The element type the collation path carries is not `Copy`, and a
    /// body written for `Copy` would not compile against it. This asks
    /// for the same answer with an owned key.
    #[test]
    fn a_non_copy_element_sorts_the_same() {
        let n = 70_000u32;
        let v: Vec<(alloc::vec::Vec<u8>, u32)> = (0..n)
            .map(|i| (((i * 7919) % 1000).to_be_bytes().to_vec(), i))
            .collect();
        let owned = |a: &(alloc::vec::Vec<u8>, u32), b: &(alloc::vec::Vec<u8>, u32)| {
            a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1))
        };
        let mut want = v.clone();
        want.sort_unstable_by(owned);
        assert_eq!(sort_total(v, plenty(4), &owned), want);
    }

    #[test]
    fn the_guc_decides_the_fan_out() {
        assert_eq!(parts_for(1_000, 8), 1, "small inputs stay serial");
        assert_eq!(parts_for(400_000, 0), 1, "the GUC can turn it off");
        assert_eq!(
            parts_for(400_000, 2),
            3,
            "PG's default of two workers sorts in three processes"
        );
        assert_eq!(parts_for(400_000, 3), 4);
        assert_eq!(
            parts_for(400_000, 64),
            MAX_PARTS,
            "a server runs more than one query"
        );
        assert_eq!(parts_for(70_000, 64), 4, "no chunk under MIN_CHUNK");
    }

    /// The process-wide cap, which is the difference between a server
    /// that fans out and a server that thrashes.
    #[test]
    fn the_process_cap_is_shared_between_sorts() {
        assert_eq!(lease_amount(0, 6, 8), 6);
        assert_eq!(lease_amount(6, 6, 8), 2, "what is left, not what was asked");
        assert_eq!(
            lease_amount(8, 6, 8),
            0,
            "a sort that finds none runs serially"
        );
        assert_eq!(lease_amount(9, 6, 8), 0, "and cannot go negative");
        assert_eq!(lease_amount(0, 6, 0), 0, "a cap of zero lets nothing out");
    }

    /// A sort that gets no lease must still sort. The cap is a limit on
    /// threads, never on answers. Asked with a process cap of zero, so
    /// the answer does not depend on what other tests are holding.
    #[test]
    fn a_sort_with_no_threads_left_still_sorts() {
        let v: Vec<(u64, u32)> = (0..70_000u32).map(|i| (u64::from(i % 997), i)).collect();
        let mut want = v.clone();
        want.sort_unstable_by(total);
        assert_eq!(
            sort_total(
                v,
                Workers {
                    per_sort: 8,
                    per_process: 0
                },
                &total
            ),
            want
        );
    }
}
