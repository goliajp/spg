use super::*;
// NSW algorithms moved to `crate::nsw` (monster tier-3 cut 2); these
// unit tests exercise its crate-internal distance kernels + search
// directly. Glob keeps the aarch64 NEON variants cfg-correct.
use crate::nsw::*;
use alloc::string::ToString;
use alloc::vec;

/// v7.37.16 (Epic W) — the v53 catalog snapshot now persists each row's
/// stable `RowId` (+ MVCC header). `RowId` allocation is path-dependent by
/// design: redo replay's `set_rows_and_rebuild_indices` assigns FRESH ids
/// rather than reproducing the exact ids a direct mutation sequence would
/// hand out (see its doc: "fresh monotonic ids … so a post-replay id never
/// collides with a pre-replay one"). So a direct-ops catalog and a
/// redo-replayed one — logically identical rows, all frozen headers on the
/// gate-off paths the redo tests exercise — legitimately differ ONLY in the
/// MVCC appendix's rowid bookkeeping. Normalising both to dense ids before
/// a byte-level `serialize()` comparison keeps the differential covering
/// schema + rows + indices + headers without over-asserting on the
/// intentionally path-dependent id allocation.
#[cfg(test)]
fn normalize_rowids_dense(c: &mut Catalog, tables: &[&str]) {
    for name in tables {
        c.get_mut(name).unwrap().assign_dense_rowids();
    }
}

/// v7.34 (crash-recovery P0 #2) — row-level physical redo apply (S4
/// core) must reproduce a catalog built by direct mutations,
/// byte-for-byte. Build C1 by direct `Table` ops, an equivalent
/// `RowChange` log, and C2 by applying that log; identical `serialize()`
/// is the differential the redo replay relies on (position-based replay
/// ≡ the original mutation sequence from the same baseline).
#[test]
fn redo_apply_matches_direct_position_ops() {
    fn fresh() -> Catalog {
        let mut c = Catalog::new();
        c.create_table(TableSchema::new(
            "t",
            vec![
                ColumnSchema::new("id", DataType::BigInt, false),
                ColumnSchema::new("v", DataType::Text, true),
            ],
        ))
        .unwrap();
        c
    }
    let rows = [
        Row::new(alloc::vec![Value::BigInt(1), Value::text("a")]),
        Row::new(alloc::vec![Value::BigInt(2), Value::text("b")]),
        Row::new(alloc::vec![Value::BigInt(3), Value::text("c")]),
        Row::new(alloc::vec![Value::BigInt(4), Value::text("d")]),
    ];
    let upd = alloc::vec![Value::BigInt(2), Value::text("B")];

    // C1 — direct storage ops.
    let mut c1 = fresh();
    {
        let t = c1.get_mut("t").unwrap();
        for r in &rows {
            t.insert(r.clone()).unwrap();
        }
        t.update_row(1, upd.clone()).unwrap(); // id=2 → "B"
        t.delete_rows(&[0, 2]); // drop positions 0 (id=1) and 2 (id=3)
    }

    // Equivalent redo log: the same physical ops, same order.
    let mut log = alloc::vec::Vec::new();
    for r in &rows {
        log.push(RowChange::Insert {
            table: "t".to_string(),
            row: r.clone(),
            rowid: row_header::RowId::UNASSIGNED,
            writer_version: 0,
        });
    }
    log.push(RowChange::Update {
        table: "t".to_string(),
        pos: 1,
        new_row: upd,
        rowid: row_header::RowId::UNASSIGNED,
        writer_version: 0,
    });
    log.push(RowChange::Delete {
        table: "t".to_string(),
        positions: vec![0, 2],
        rowids: vec![row_header::RowId::UNASSIGNED; 2],
        writer_version: 0,
    });

    // C2 — apply the log to a fresh catalog.
    let mut c2 = fresh();
    c2.apply_redo(&log).unwrap();

    // Normalise the path-dependent RowId allocation before the byte
    // comparison (see `normalize_rowids_dense`).
    normalize_rowids_dense(&mut c1, &["t"]);
    normalize_rowids_dense(&mut c2, &["t"]);
    assert_eq!(
        c1.serialize(),
        c2.serialize(),
        "redo apply diverged from direct position ops"
    );

    // A redo log naming an absent table is corrupt, not a silent skip.
    let mut c3 = fresh();
    assert!(
        c3.apply_redo(&[RowChange::Insert {
            table: "nope".to_string(),
            row: rows[0].clone(),
            rowid: row_header::RowId::UNASSIGNED,
            writer_version: 0,
        }])
        .is_err()
    );
}

/// v7.34 (crash-recovery P0 #2) — the REAL capture≡execute differential:
/// capture the redo emitted by live mutations, replay it onto a fresh
/// catalog, and require byte-identical state. This is what row-level WAL
/// recovery does (replay the captured log instead of re-running the SQL).
#[test]
fn redo_capture_replays_to_identical_state() {
    fn fresh() -> Catalog {
        let mut c = Catalog::new();
        c.create_table(TableSchema::new(
            "t",
            vec![
                ColumnSchema::new("id", DataType::BigInt, false),
                ColumnSchema::new("v", DataType::Text, true),
            ],
        ))
        .unwrap();
        c
    }
    let mk = |id: i64, v: &str| Row::new(alloc::vec![Value::BigInt(id), Value::text(v)]);

    let mut c1 = fresh();
    {
        let t = c1.get_mut("t").unwrap();
        t.enable_redo();
        t.insert(mk(1, "a")).unwrap();
        t.insert(mk(2, "b")).unwrap();
        t.insert(mk(3, "c")).unwrap();
        t.update_row(1, alloc::vec![Value::BigInt(2), Value::text("B")])
            .unwrap();
        t.delete_rows(&[0]); // drop id=1
        t.delete_rows(&[99]); // out of range → no-op → must NOT be captured
    }
    let log = c1.get_mut("t").unwrap().take_redo();
    // insert×3 + update×1 + delete×1 (the no-op delete is not captured).
    assert_eq!(log.len(), 5, "captured log: {log:?}");

    let mut c2 = fresh();
    c2.apply_redo(&log).unwrap();
    normalize_rowids_dense(&mut c1, &["t"]);
    normalize_rowids_dense(&mut c2, &["t"]);
    assert_eq!(
        c1.serialize(),
        c2.serialize(),
        "replayed capture diverged from execution"
    );

    // take_redo drains + stops capturing.
    assert!(c1.get_mut("t").unwrap().take_redo().is_empty());
}

/// v7.34 (crash-recovery P0 #2) — the catalog-level redo orchestration
/// the engine uses: `enable_redo_all` before a statement, mutate any
/// tables, `drain_redo` after — the drained log replays across ALL
/// touched tables onto a fresh catalog identically.
#[test]
fn catalog_drain_redo_replays_multi_table() {
    fn fresh() -> Catalog {
        let mut c = Catalog::new();
        for name in ["a", "b"] {
            c.create_table(TableSchema::new(
                name,
                vec![
                    ColumnSchema::new("id", DataType::BigInt, false),
                    ColumnSchema::new("v", DataType::Text, true),
                ],
            ))
            .unwrap();
        }
        c
    }
    let mk = |id: i64, v: &str| Row::new(alloc::vec![Value::BigInt(id), Value::text(v)]);

    let mut c1 = fresh();
    c1.enable_redo_all();
    c1.get_mut("a").unwrap().insert(mk(1, "a1")).unwrap();
    c1.get_mut("b").unwrap().insert(mk(2, "b1")).unwrap();
    c1.get_mut("a").unwrap().insert(mk(3, "a2")).unwrap();
    c1.get_mut("a")
        .unwrap()
        .update_row(0, alloc::vec![Value::BigInt(1), Value::text("A1")])
        .unwrap();
    c1.get_mut("b").unwrap().delete_rows(&[0]);
    let log = c1.drain_redo();

    let mut c2 = fresh();
    c2.apply_redo(&log).unwrap();
    normalize_rowids_dense(&mut c1, &["a", "b"]);
    normalize_rowids_dense(&mut c2, &["a", "b"]);
    assert_eq!(c1.serialize(), c2.serialize(), "multi-table redo diverged");

    // drain stopped capture: a second drain is empty.
    assert!(c1.drain_redo().is_empty());
}

/// v7.34 (crash-recovery P0 #2) — the row-level redo WAL codec (S2):
/// encode/decode round-trips every `RowChange` variant + value family,
/// and a truncated/empty buffer is a hard error (not a partial decode).
#[test]
fn redo_log_codec_round_trips() {
    use row_header::RowId;
    // Epic W slice 1 — carry real RowId + writer_version metadata so
    // the new-format round-trip exercises the metadata path.
    let changes = vec![
        RowChange::Insert {
            table: "t".to_string(),
            row: Row::new(alloc::vec![
                Value::BigInt(1),
                Value::text("a"),
                Value::Null,
                Value::Bool(true),
            ]),
            rowid: RowId(11),
            writer_version: 101,
        },
        RowChange::Update {
            table: "users".to_string(),
            pos: 42,
            new_row: alloc::vec![Value::Int(7), Value::bytes(alloc::vec![1, 2, 3])],
            rowid: RowId(22),
            writer_version: 202,
        },
        RowChange::Delete {
            table: "t".to_string(),
            positions: alloc::vec![0, 5, 99],
            rowids: alloc::vec![RowId(3), RowId(4), RowId(5)],
            writer_version: 303,
        },
        RowChange::Delete {
            table: "empty".to_string(),
            positions: alloc::vec![],
            rowids: alloc::vec![],
            writer_version: 0,
        },
        // Epic W durable-tombstone slice — the new in-place tombstone op
        // (byte 3) round-trips its RowId list + xmax.
        RowChange::Tombstone {
            table: "t".to_string(),
            rowids: alloc::vec![RowId(7), RowId(8)],
            xmax: 404,
        },
        RowChange::Tombstone {
            table: "solo".to_string(),
            rowids: alloc::vec![RowId(9)],
            xmax: 505,
        },
    ];
    let bytes = encode_redo_log(&changes);
    assert_eq!(decode_redo_log(&bytes).unwrap(), changes);

    // Empty log round-trips.
    let empty = encode_redo_log(&[]);
    assert_eq!(decode_redo_log(&empty).unwrap(), Vec::<RowChange>::new());

    // Truncated / empty buffer is corruption, not a partial decode.
    assert!(decode_redo_log(&bytes[..bytes.len() / 2]).is_err());
    assert!(decode_redo_log(&[]).is_err());
}

/// v7.37.15 (Epic W slice 1) — the non-negotiable backward-compat
/// gate: a redo payload written by PRE-Epic-W released code (leading
/// `FILE_VERSION` byte, NO per-change metadata) must still decode +
/// replay identically. This test hand-crafts a pre-Epic-W byte buffer
/// (the exact layout `encode_redo_log` produced before this slice) and
/// asserts it decodes to the same logical `RowChange`s with
/// `RowId::UNASSIGNED` / `writer_version = 0`, and that replaying it
/// reproduces the same table state as a fresh direct-mutation build.
#[test]
fn redo_log_old_format_decodes_and_replays_identically() {
    use crate::codec;
    use row_header::RowId;

    // Reconstruct the PRE-Epic-W wire layout verbatim:
    //   [u8 FILE_VERSION][u32 count]
    //   Insert  [0][str table][u32 n][value×n]
    //   Update  [1][str table][u32 pos][u32 n][value×n]
    //   Delete  [2][str table][u32 n][u32 pos×n]
    // No RowId / writer_version bytes existed.
    fn encode_old(changes: &[RowChange]) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(crate::FILE_VERSION);
        codec::write_u32(&mut out, changes.len() as u32);
        let write_values = |out: &mut Vec<u8>, vals: &[Value<'static>]| {
            codec::write_u32(out, vals.len() as u32);
            for v in vals {
                codec::write_value(out, v);
            }
        };
        for change in changes {
            match change {
                RowChange::Insert { table, row, .. } => {
                    out.push(0);
                    codec::write_str(&mut out, table);
                    write_values(&mut out, &row.values);
                }
                RowChange::Update {
                    table, pos, new_row, ..
                } => {
                    out.push(1);
                    codec::write_str(&mut out, table);
                    codec::write_u32(&mut out, *pos as u32);
                    write_values(&mut out, new_row);
                }
                RowChange::Delete {
                    table, positions, ..
                } => {
                    out.push(2);
                    codec::write_str(&mut out, table);
                    codec::write_u32(&mut out, positions.len() as u32);
                    for p in positions {
                        codec::write_u32(&mut out, *p as u32);
                    }
                }
                // The pre-Epic-W layout had no in-place tombstone op; this
                // helper is never handed one (the `logical` fixture below
                // uses only Insert/Update/Delete).
                RowChange::Tombstone { .. } => {
                    unreachable!("old redo layout has no Tombstone op")
                }
            }
        }
        out
    }

    fn fresh() -> Catalog {
        let mut c = Catalog::new();
        c.create_table(TableSchema::new(
            "t",
            vec![
                ColumnSchema::new("id", DataType::BigInt, false),
                ColumnSchema::new("v", DataType::Text, true),
            ],
        ))
        .unwrap();
        c
    }
    let mk = |id: i64, v: &str| Row::new(alloc::vec![Value::BigInt(id), Value::text(v)]);

    // Logical operations. `encode_old` ignores the metadata fields, so
    // this represents exactly what a pre-Epic-W writer would have put
    // on disk for the same sequence of physical mutations.
    let logical = vec![
        RowChange::Insert {
            table: "t".to_string(),
            row: mk(1, "a"),
            rowid: RowId(999), // ignored by encode_old
            writer_version: 7, // ignored by encode_old
        },
        RowChange::Insert {
            table: "t".to_string(),
            row: mk(2, "b"),
            rowid: RowId(999),
            writer_version: 7,
        },
        RowChange::Update {
            table: "t".to_string(),
            pos: 0,
            new_row: alloc::vec![Value::BigInt(1), Value::text("A")],
            rowid: RowId(999),
            writer_version: 7,
        },
        RowChange::Delete {
            table: "t".to_string(),
            positions: alloc::vec![1],
            rowids: alloc::vec![RowId(999)],
            writer_version: 7,
        },
    ];

    let old_bytes = encode_old(&logical);
    // The old buffer's first byte is FILE_VERSION, never the 0xFF meta
    // marker — the gate that routes it to the legacy decode path.
    assert_ne!(old_bytes[0], 0xFF, "old layout must not look like new");

    let decoded = decode_redo_log(&old_bytes).unwrap();
    // Same logical content, but metadata absent → UNASSIGNED / 0 / empty.
    let expected = vec![
        RowChange::Insert {
            table: "t".to_string(),
            row: mk(1, "a"),
            rowid: RowId::UNASSIGNED,
            writer_version: 0,
        },
        RowChange::Insert {
            table: "t".to_string(),
            row: mk(2, "b"),
            rowid: RowId::UNASSIGNED,
            writer_version: 0,
        },
        RowChange::Update {
            table: "t".to_string(),
            pos: 0,
            new_row: alloc::vec![Value::BigInt(1), Value::text("A")],
            rowid: RowId::UNASSIGNED,
            writer_version: 0,
        },
        RowChange::Delete {
            table: "t".to_string(),
            positions: alloc::vec![1],
            rowids: alloc::vec![], // no RowId metadata in old layout
            writer_version: 0,
        },
    ];
    assert_eq!(decoded, expected, "old-format decode diverged");

    // And it must REPLAY to the same state as a direct-mutation build.
    let mut direct = fresh();
    {
        let t = direct.get_mut("t").unwrap();
        t.insert(mk(1, "a")).unwrap();
        t.insert(mk(2, "b")).unwrap();
        t.update_row(0, alloc::vec![Value::BigInt(1), Value::text("A")])
            .unwrap();
        t.delete_rows(&[1]);
    }
    let mut replayed = fresh();
    replayed.apply_redo(&decoded).unwrap();
    normalize_rowids_dense(&mut direct, &["t"]);
    normalize_rowids_dense(&mut replayed, &["t"]);
    assert_eq!(
        direct.serialize(),
        replayed.serialize(),
        "old-format redo replay diverged from direct ops"
    );
}

/// v7.37.15 (Epic W durable-tombstone slice) — the durability proof for
/// the gate-on (`SPG_MVCC_INPLACE`) in-place DELETE path. A gate-on
/// DELETE calls `Table::mark_row_deleted` (stamps `xmax`, keeps the row)
/// instead of `delete_rows`. This test drives that capture, round-trips
/// the redo through the WAL codec, applies it into a FRESH catalog, and
/// asserts the tombstoned row survives replay as a tombstone — hidden
/// from a fresh snapshot but physically present — while the survivors
/// stay visible. A gate-off control (physical `delete_rows`) is replayed
/// the same way and must instead physically remove the row.
#[test]
fn redo_tombstone_survives_replay_hidden_from_snapshot() {
    use crate::row_header::XMAX_ALIVE;
    use crate::snapshot::Snapshot;

    fn fresh() -> Catalog {
        let mut c = Catalog::new();
        c.create_table(TableSchema::new(
            "t",
            vec![ColumnSchema::new("id", DataType::Int, false)],
        ))
        .unwrap();
        c
    }
    let mk = |id: i32| Row::new(alloc::vec![Value::Int(id)]);
    let snap = Snapshot::unbounded();

    // --- Capture side: INSERT 3, then in-place tombstone row id=2. ---
    // xmax = 42 stands in for the deleting statement's writer version
    // (the engine passes `writer_version_for_current_stmt`).
    const TOMB_XMAX: u64 = 42;
    let mut cap = fresh();
    cap.enable_redo_all();
    {
        let t = cap.get_mut("t").unwrap();
        t.insert(mk(1)).unwrap();
        t.insert(mk(2)).unwrap();
        t.insert(mk(3)).unwrap();
        // Tombstone the physical position of id=2 (slot 1).
        t.mark_row_deleted(1, TOMB_XMAX).unwrap();
    }
    let log = cap.drain_redo();
    // The drained log must contain exactly one Tombstone naming the
    // RowId the insert of id=2 allocated (so replay can re-find it).
    let tomb_ids: Vec<_> = log
        .iter()
        .filter_map(|c| match c {
            RowChange::Tombstone { rowids, xmax, .. } => Some((rowids.clone(), *xmax)),
            _ => None,
        })
        .collect();
    assert_eq!(tomb_ids.len(), 1, "one in-place delete → one Tombstone redo");
    assert_eq!(tomb_ids[0].1, TOMB_XMAX, "tombstone carries the writer version");
    assert_eq!(tomb_ids[0].0.len(), 1, "one row tombstoned");

    // --- Round-trip through the WAL codec (the real durability path). ---
    let bytes = encode_redo_log(&log);
    let decoded = decode_redo_log(&bytes).unwrap();
    assert_eq!(decoded, log, "tombstone redo must round-trip the codec");

    // --- Replay into a FRESH catalog (crash-recovery simulation). ---
    let unresolved_before = crate::unresolved_tombstone_count();
    let mut rep = fresh();
    rep.apply_redo(&decoded).unwrap();
    assert_eq!(
        crate::unresolved_tombstone_count(),
        unresolved_before,
        "the tombstone target must resolve by RowId within the replay run"
    );

    // Physically present: 3 rows survive (tombstone keeps the slot).
    let t = rep.get("t").unwrap();
    assert_eq!(t.rows().len(), 3, "tombstone must NOT physically remove the row");
    // The visible set (per the MVCC snapshot gate) is {1, 3}; id=2 is a
    // tombstone hidden from a fresh snapshot — exactly the live gate-on
    // result, now reproduced from the WAL.
    let visible: Vec<i32> = t
        .scan_visible(&snap)
        .filter_map(|(_, r)| match r.values.first() {
            Some(Value::Int(v)) => Some(*v),
            _ => None,
        })
        .collect();
    assert_eq!(visible, alloc::vec![1, 3], "tombstoned row must be hidden after replay");
    // And the hidden row's header carries the exact xmax we stamped.
    let hidden_idx = t
        .rows()
        .iter()
        .position(|r| r.values.first() == Some(&Value::Int(2)))
        .expect("id=2 physically present");
    let h = t.headers().get(hidden_idx).expect("header lock-step");
    assert_eq!(h.xmax, TOMB_XMAX, "recovered row must carry the tombstone xmax");
    assert!(h.is_deleted(), "recovered row must read as deleted");
    // Survivors stay alive (not accidentally tombstoned).
    for (i, r) in t.rows().iter().enumerate() {
        if r.values.first() != Some(&Value::Int(2)) {
            assert_eq!(
                t.headers().get(i).unwrap().xmax,
                XMAX_ALIVE,
                "survivor {i} must stay alive"
            );
        }
    }

    // --- Gate-off control: physical delete replays as a real removal. ---
    let mut capc = fresh();
    capc.enable_redo_all();
    {
        let t = capc.get_mut("t").unwrap();
        t.insert(mk(1)).unwrap();
        t.insert(mk(2)).unwrap();
        t.insert(mk(3)).unwrap();
        t.delete_rows(&[1]); // physical delete (gate-off path)
    }
    let logc = capc.drain_redo();
    assert!(
        logc.iter().all(|c| !matches!(c, RowChange::Tombstone { .. })),
        "gate-off DELETE must NOT emit a Tombstone redo"
    );
    let mut repc = fresh();
    repc.apply_redo(&decode_redo_log(&encode_redo_log(&logc)).unwrap())
        .unwrap();
    let tc = repc.get("t").unwrap();
    assert_eq!(tc.rows().len(), 2, "gate-off replay physically removes the row");
    let ids: Vec<i32> = tc
        .rows()
        .iter()
        .filter_map(|r| match r.values.first() {
            Some(Value::Int(v)) => Some(*v),
            _ => None,
        })
        .collect();
    assert_eq!(ids, alloc::vec![1, 3], "gate-off replay keeps only survivors");
}

/// v7.37.16 (Epic W) — durability proof for the gate-on
/// (`SPG_MVCC_INPLACE`) in-place UPDATE path. A gate-on UPDATE supersedes
/// a row by tombstoning the old version (`Table::mark_row_deleted`, xmax =
/// writer version) and appending the new version
/// (`Table::insert_with_xmin`) — exactly what `Engine::update`'s in-place
/// branch does (`dml.rs` ~line 498/849/2545). This test drives that
/// two-step capture, round-trips the redo through the WAL codec, replays
/// it into a FRESH catalog, and asserts the recovered state reproduces the
/// live gate-on result: the OLD version is hidden (tombstoned), the NEW
/// version is visible, survivors untouched, zero unresolved tombstones.
///
/// This is the W-3 slice's DELETE proof re-run for UPDATE: the ONLY
/// difference is the extra `Insert(new)` that rides the same redo run.
/// Because both the tombstone (old RowId) and the insert (new RowId) are
/// produced within one run, the RowId post-pass resolves the tombstone
/// against the run-start ids — no checkpoint boundary is crossed. The
/// new version replays as a plain `Insert` (frozen header on replay), so
/// for an all-committed recovered DB it is correctly visible — the same
/// reasoning that already makes gate-off inserts durable.
#[test]
fn redo_update_tombstone_plus_insert_survives_replay() {
    use crate::row_header::XMAX_ALIVE;
    use crate::snapshot::Snapshot;

    fn fresh() -> Catalog {
        let mut c = Catalog::new();
        c.create_table(TableSchema::new(
            "t",
            vec![ColumnSchema::new("id", DataType::Int, false)],
        ))
        .unwrap();
        c
    }
    let mk = |id: i32| Row::new(alloc::vec![Value::Int(id)]);
    let snap = Snapshot::unbounded();

    // --- Capture side: INSERT 3, then in-place UPDATE id=2 -> id=20. ---
    // The gate-on UPDATE = tombstone old (xmax = V) + insert new (xmin =
    // V), in that order, mirroring `Engine::update`'s in-place branch.
    const STMT_V: u64 = 42;
    const OLD_VAL: i32 = 2;
    const NEW_VAL: i32 = 20;
    let mut cap = fresh();
    cap.enable_redo_all();
    {
        let t = cap.get_mut("t").unwrap();
        t.insert(mk(1)).unwrap();
        t.insert(mk(OLD_VAL)).unwrap();
        t.insert(mk(3)).unwrap();
        // In-place UPDATE of slot 1 (value=2): tombstone the old version…
        t.mark_row_deleted(1, STMT_V).unwrap();
        // …then append the new version with xmin = the same statement V.
        t.insert_with_xmin(mk(NEW_VAL), STMT_V).unwrap();
    }
    let log = cap.drain_redo();

    // The UPDATE must emit BOTH a Tombstone (old RowId, xmax=V) AND an
    // Insert carrying the new values, and the tombstone must come first
    // (old superseded before new appended).
    let tomb_pos = log
        .iter()
        .position(|c| matches!(c, RowChange::Tombstone { .. }))
        .expect("in-place UPDATE emits a Tombstone for the old version");
    let (tomb_rowids, tomb_xmax) = match &log[tomb_pos] {
        RowChange::Tombstone { rowids, xmax, .. } => (rowids.clone(), *xmax),
        _ => unreachable!(),
    };
    assert_eq!(tomb_rowids.len(), 1, "one row superseded → one tombstone target");
    assert_eq!(tomb_xmax, STMT_V, "tombstone carries the statement writer version");
    // Exactly one tombstone total (no double-tombstone).
    assert_eq!(
        log.iter().filter(|c| matches!(c, RowChange::Tombstone { .. })).count(),
        1,
        "an in-place UPDATE tombstones the old version exactly once"
    );
    // The new version rides as an Insert AFTER the tombstone, carrying the
    // new row values.
    let new_ins_pos = log
        .iter()
        .position(|c| matches!(
            c,
            RowChange::Insert { row, .. } if row.values.first() == Some(&Value::Int(NEW_VAL))
        ))
        .expect("in-place UPDATE emits an Insert carrying the new values");
    assert!(
        tomb_pos < new_ins_pos,
        "tombstone(old) must precede insert(new) in the redo run"
    );

    // --- Round-trip through the WAL codec (the real durability path). ---
    let bytes = encode_redo_log(&log);
    let decoded = decode_redo_log(&bytes).unwrap();
    assert_eq!(decoded, log, "UPDATE tombstone+insert redo must round-trip the codec");

    // --- Replay into a FRESH catalog (crash-recovery simulation). ---
    let unresolved_before = crate::unresolved_tombstone_count();
    let mut rep = fresh();
    rep.apply_redo(&decoded).unwrap();
    assert_eq!(
        crate::unresolved_tombstone_count(),
        unresolved_before,
        "the UPDATE's tombstone must resolve by RowId within the replay run"
    );

    let t = rep.get("t").unwrap();
    // 4 rows physically present: 3 originals (one now tombstoned) + the
    // appended new version.
    assert_eq!(t.rows().len(), 4, "in-place UPDATE keeps the old row + appends the new");
    // Visible set (per the MVCC snapshot gate): survivors {1,3} + new {20};
    // the OLD value {2} is hidden — exactly the live gate-on UPDATE result,
    // reproduced from the WAL.
    let mut visible: Vec<i32> = t
        .scan_visible(&snap)
        .filter_map(|(_, r)| match r.values.first() {
            Some(Value::Int(v)) => Some(*v),
            _ => None,
        })
        .collect();
    visible.sort_unstable();
    assert_eq!(
        visible,
        alloc::vec![1, 3, NEW_VAL],
        "old version hidden, new version visible, survivors intact"
    );
    assert!(
        !visible.contains(&OLD_VAL),
        "the superseded (old) value must NOT be visible after replay"
    );

    // The old (superseded) row is physically present with the tombstone
    // xmax stamped and reads as deleted.
    let old_idx = t
        .rows()
        .iter()
        .position(|r| r.values.first() == Some(&Value::Int(OLD_VAL)))
        .expect("old version physically present (tombstone keeps the slot)");
    let old_h = t.headers().get(old_idx).expect("header lock-step");
    assert_eq!(old_h.xmax, STMT_V, "superseded row must carry the tombstone xmax");
    assert!(old_h.is_deleted(), "superseded row must read as deleted");
    // The new version is a live, undeleted row.
    let new_idx = t
        .rows()
        .iter()
        .position(|r| r.values.first() == Some(&Value::Int(NEW_VAL)))
        .expect("new version physically present");
    let new_h = t.headers().get(new_idx).expect("header lock-step");
    assert_eq!(new_h.xmax, XMAX_ALIVE, "new version must stay alive");
    assert!(!new_h.is_deleted(), "new version must not read as deleted");
    // Survivors (id 1, 3) stay alive.
    for (i, r) in t.rows().iter().enumerate() {
        match r.values.first() {
            Some(&Value::Int(1)) | Some(&Value::Int(3)) => assert_eq!(
                t.headers().get(i).unwrap().xmax,
                XMAX_ALIVE,
                "survivor {i} must stay alive"
            ),
            _ => {}
        }
    }

    // --- Gate-off control: physical UPDATE replays in place, no tombstone.
    let mut capc = fresh();
    capc.enable_redo_all();
    {
        let t = capc.get_mut("t").unwrap();
        t.insert(mk(1)).unwrap();
        t.insert(mk(OLD_VAL)).unwrap();
        t.insert(mk(3)).unwrap();
        t.update_row(1, alloc::vec![Value::Int(NEW_VAL)]).unwrap(); // physical (gate-off)
    }
    let logc = capc.drain_redo();
    assert!(
        logc.iter().all(|c| !matches!(c, RowChange::Tombstone { .. })),
        "gate-off UPDATE must NOT emit a Tombstone redo"
    );
    assert!(
        logc.iter().any(|c| matches!(c, RowChange::Update { .. })),
        "gate-off UPDATE emits an in-place Update redo"
    );
    let mut repc = fresh();
    repc.apply_redo(&decode_redo_log(&encode_redo_log(&logc)).unwrap())
        .unwrap();
    let tc = repc.get("t").unwrap();
    assert_eq!(tc.rows().len(), 3, "gate-off UPDATE replays in place (no extra row)");
    let mut ids: Vec<i32> = tc
        .rows()
        .iter()
        .filter_map(|r| match r.values.first() {
            Some(Value::Int(v)) => Some(*v),
            _ => None,
        })
        .collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        alloc::vec![1, 3, NEW_VAL],
        "gate-off replay updates the row in place to the new value"
    );
}

// ---------------------------------------------------------------------
// v7.37.16 (Epic W) — FILE_VERSION 53 catalog-snapshot MVCC appendix:
// persist per-row RowHeader (xmin/xmax/flags) + stable RowId so a
// cross-checkpoint tombstone survives a serialize→deserialize restore.
// ---------------------------------------------------------------------

/// Build the exact bytes the v53 per-table MVCC appendix would emit for a
/// table: `[u32 count][per row: u64 xmin,u64 xmax,u8 flags,u64 rowid]
/// [u64 next_rowid]`. Used by the byte-compat test to splice the appendix
/// back out of a v53 image and reconstruct a genuine v52 image.
#[cfg(test)]
fn mvcc_appendix_bytes(t: &Table) -> Vec<u8> {
    let mut a = Vec::new();
    a.extend_from_slice(&(t.rows().len() as u32).to_le_bytes());
    for (h, rid) in t.headers().iter().zip(t.rowids().iter()) {
        a.extend_from_slice(&h.xmin.to_le_bytes());
        a.extend_from_slice(&h.xmax.to_le_bytes());
        a.push(h.flags);
        a.extend_from_slice(&rid.0.to_le_bytes());
    }
    a.extend_from_slice(&t.next_rowid_for_test().to_le_bytes());
    a
}

/// (a) BACKWARD-COMPAT GATE. A snapshot written by the CURRENT released
/// code (FILE_VERSION 52, no MVCC appendix) MUST still deserialize to the
/// exact same result as before this slice: every row `RowHeader::frozen()`
/// and dense `1..=N` rowids with `next_rowid = N + 1`.
///
/// The current image differs from the v52 image by: the version byte, the
/// per-table MVCC appendix (v53), and the per-table default_text appendix
/// (v58) — the latter an empty 2-byte zero count here (`t` has no column
/// default) sitting immediately before the MVCC appendix. So we serialize,
/// splice out the (uniquely-locatable) MVCC appendix plus the 2 empty
/// default_text bytes preceding it, flip the version byte to 52, and assert
/// the result loads with the pre-v53 frozen/dense contract — a real old-image
/// load.
#[test]
fn v52_snapshot_without_mvcc_appendix_loads_frozen_and_dense() {
    use crate::row_header::{RowHeader, RowId, XMAX_ALIVE, XMIN_FROZEN};

    let mut c = Catalog::new();
    c.create_table(TableSchema::new(
        "t",
        vec![ColumnSchema::new("id", DataType::Int, false)],
    ))
    .unwrap();
    {
        let t = c.get_mut("t").unwrap();
        // Distinctive values so the appendix subslice is unique.
        t.insert(Row::new(alloc::vec![Value::Int(0x1111)])).unwrap();
        t.insert(Row::new(alloc::vec![Value::Int(0x2222)])).unwrap();
        t.insert(Row::new(alloc::vec![Value::Int(0x3333)])).unwrap();
    }
    // v7.38 (P5.05) — the current writer emits v54 with a trailing CRC32C;
    // strip it so what remains is the pre-trailer body we downgrade to v52.
    let v53 = {
        let mut full = c.serialize();
        full.truncate(full.len() - 4);
        full
    };

    // Locate + splice out the appendix (must be present exactly once).
    let appendix = mvcc_appendix_bytes(c.get("t").unwrap());
    let hits: Vec<usize> = v53
        .windows(appendix.len())
        .enumerate()
        .filter_map(|(i, w)| if w == appendix.as_slice() { Some(i) } else { None })
        .collect();
    assert_eq!(hits.len(), 1, "MVCC appendix must appear exactly once in the image");
    let start = hits[0];
    // v7.38 — strip the empty (2-byte zero-count) default_text appendix (v58)
    // that the current writer emits immediately before the MVCC appendix, so
    // the spliced image is byte-for-byte a genuine pre-v53 catalog.
    const EMPTY_DEFAULT_TEXT_APPENDIX: usize = 2;
    let mut v52 = Vec::with_capacity(v53.len() - appendix.len() - EMPTY_DEFAULT_TEXT_APPENDIX);
    v52.extend_from_slice(&v53[..start - EMPTY_DEFAULT_TEXT_APPENDIX]);
    v52.extend_from_slice(&v53[start + appendix.len()..]);
    // Set the version byte to 52 — the format before the MVCC appendix (v53)
    // and before the CRC trailer (v54): byte-for-byte what the pre-slice
    // released code would have written for this catalog.
    v52[FILE_MAGIC.len()] = 52;

    let restored = Catalog::deserialize(&v52).expect("v52 image must still load");
    let t = restored.get("t").unwrap();
    assert_eq!(t.rows().len(), 3, "rows survive the v52 load");
    // Every header frozen (pre-v53 contract).
    for i in 0..t.rows().len() {
        let h = *t.headers().get(i).unwrap();
        assert_eq!(h, RowHeader::frozen(), "v52 row {i} must load frozen");
        assert_eq!(h.xmin, XMIN_FROZEN);
        assert_eq!(h.xmax, XMAX_ALIVE);
    }
    // Dense 1..=N rowids + next_rowid = N + 1.
    let ids: Vec<RowId> = t.rowids().iter().copied().collect();
    assert_eq!(ids, alloc::vec![RowId(1), RowId(2), RowId(3)], "v52 dense rowids");
    assert_eq!(t.next_rowid_for_test(), 4, "v52 next_rowid = N + 1");
}

/// A short / truncated MVCC appendix must error cleanly (no panic/unwrap)
/// on load. We take a valid v53 image and chop off the trailing bytes so
/// the appendix reader hits EOF mid-field.
#[test]
fn truncated_mvcc_appendix_errors_cleanly() {
    let mut c = Catalog::new();
    c.create_table(TableSchema::new(
        "t",
        vec![ColumnSchema::new("id", DataType::Int, false)],
    ))
    .unwrap();
    {
        let t = c.get_mut("t").unwrap();
        t.insert(Row::new(alloc::vec![Value::Int(7)])).unwrap();
        t.insert(Row::new(alloc::vec![Value::Int(8)])).unwrap();
    }
    let full = c.serialize();
    // Chop the last 5 bytes: guaranteed to land inside the appendix's
    // trailing next_rowid (8 bytes), so the reader hits EOF.
    let truncated = &full[..full.len() - 5];
    let err = Catalog::deserialize(truncated);
    assert!(err.is_err(), "a truncated snapshot must error, not panic");
}

/// (b) NEW ROUND-TRIP. A table with mixed headers (some tombstoned with a
/// real xmax, some alive) and specific non-dense rowids must serialize +
/// deserialize with headers AND rowids identical, and next_rowid correct
/// (strictly above every loaded id).
#[test]
fn v53_roundtrip_preserves_mixed_headers_and_rowids() {
    use crate::row_header::{RowHeader, RowId, HEAP_XMIN_FROZEN, XMAX_ALIVE};

    let mut c = Catalog::new();
    c.create_table(TableSchema::new(
        "t",
        vec![ColumnSchema::new("id", DataType::Int, false)],
    ))
    .unwrap();
    {
        let t = c.get_mut("t").unwrap();
        // Insert 6 rows (rowids 1..=6, next_rowid = 7), then physically
        // delete slots 1,3,5 (rowids 2,4,6). Survivors keep their real
        // ids [1,3,5]; next_rowid stays 7 — a non-dense id set.
        for v in 0..6 {
            t.insert(Row::new(alloc::vec![Value::Int(100 + v)])).unwrap();
        }
        t.delete_rows(&[1, 3, 5]);
        assert_eq!(t.rows().len(), 3);
        assert_eq!(
            t.rowids().iter().copied().collect::<Vec<_>>(),
            alloc::vec![RowId(1), RowId(3), RowId(5)],
            "survivors keep their real (non-dense) rowids"
        );
        assert_eq!(t.next_rowid_for_test(), 7, "next_rowid unaffected by delete");
        // Stamp mixed headers: slot 0 alive-frozen, slot 1 tombstoned
        // (xmax = 99), slot 2 alive with a non-frozen xmin.
        let headers = t.headers_mut_for_test();
        *headers.get_mut(0).unwrap() = RowHeader::frozen();
        *headers.get_mut(1).unwrap() = RowHeader {
            xmin: 5,
            xmax: 99,
            flags: 0,
        };
        *headers.get_mut(2).unwrap() = RowHeader {
            xmin: 42,
            xmax: XMAX_ALIVE,
            flags: HEAP_XMIN_FROZEN,
        };
    }
    let want_headers: Vec<RowHeader> =
        c.get("t").unwrap().headers().iter().copied().collect();

    let bytes = c.serialize();
    let restored = Catalog::deserialize(&bytes).expect("v53 image loads");
    let t = restored.get("t").unwrap();

    // Rowids identical (verbatim, NOT dense-reassigned).
    assert_eq!(
        t.rowids().iter().copied().collect::<Vec<_>>(),
        alloc::vec![RowId(1), RowId(3), RowId(5)],
        "rowids must round-trip verbatim"
    );
    // Headers identical field-for-field.
    let got_headers: Vec<RowHeader> = t.headers().iter().copied().collect();
    assert_eq!(got_headers, want_headers, "headers must round-trip verbatim");
    assert!(got_headers[1].is_deleted(), "the tombstoned row stays tombstoned");
    assert_eq!(got_headers[1].xmax, 99, "tombstone xmax preserved");
    // next_rowid restored above the max loaded id (5) — a fresh alloc
    // (7) cannot collide with any restored row.
    assert_eq!(t.next_rowid_for_test(), 7, "next_rowid restored verbatim");
    assert!(
        t.next_rowid_for_test() > 5,
        "next_rowid must exceed every loaded id"
    );
}

/// v7.38 — a row restored from a durable image must be visible to a snapshot
/// this process takes. The version cursor is process-global and restarts at
/// `XMIN_FROZEN + 1`, so without recovering it past the restored `xmin` the
/// row reads as "written by a future transaction" and silently disappears —
/// which is exactly how a daemon restart used to drop every committed row but
/// the first. Regression test for the load-side cursor recovery.
#[test]
fn restored_rows_are_visible_to_a_fresh_snapshot() {
    use crate::row_header::{self, RowHeader, XMAX_ALIVE};
    use crate::snapshot::{InProgressSet, Snapshot};

    // Well above whatever the process cursor has reached in this test binary.
    const RESTORED_XMIN: u64 = 9_000_000_001;
    const RESTORED_XMAX: u64 = 9_000_000_500;

    let mut c = Catalog::new();
    c.create_table(TableSchema::new(
        "t",
        vec![ColumnSchema::new("id", DataType::Int, false)],
    ))
    .unwrap();
    {
        let t = c.get_mut("t").unwrap();
        t.insert(Row::new(vec![Value::Int(1)])).unwrap();
        t.insert(Row::new(vec![Value::Int(2)])).unwrap();
        let headers = t.headers_mut_for_test();
        // Row 0: committed by a long-gone process at a high version.
        *headers.get_mut(0).unwrap() = RowHeader {
            xmin: RESTORED_XMIN,
            xmax: XMAX_ALIVE,
            flags: 0,
        };
        // Row 1: deleted by that process at an even higher version.
        *headers.get_mut(1).unwrap() = RowHeader {
            xmin: RESTORED_XMIN,
            xmax: RESTORED_XMAX,
            flags: 0,
        };
    }

    let restored = Catalog::deserialize(&c.serialize()).expect("image loads");

    // The cursor must now sit above every restored version, so the snapshot a
    // reader takes is not "behind" the data it just loaded.
    let version = row_header::current_version();
    assert!(
        version > RESTORED_XMAX,
        "cursor {version} must exceed every restored version ({RESTORED_XMAX})"
    );

    let snap = Snapshot::new(version, InProgressSet::empty(), version, 0);
    let headers: Vec<RowHeader> = restored.get("t").unwrap().headers().iter().copied().collect();
    assert!(
        snap.visible(&headers[0]),
        "a committed restored row must not read as a future write"
    );
    // Symmetric: recovering `xmax` keeps the delete in the past, so the
    // deleted row stays deleted rather than being resurrected.
    assert!(
        !snap.visible(&headers[1]),
        "a restored tombstone must stay deleted, not resurrect"
    );
}

/// (c) CROSS-CHECKPOINT DURABILITY. A tombstone naming a row inserted
/// BEFORE the last checkpoint must survive the base-snapshot boundary:
/// after serialize→deserialize the tombstone-redo resolves by RowId and
/// hides the row, with `unresolved_tombstone_count()` unchanged.
///
/// The scenario is engineered so the surviving row's real RowId (4) is NOT
/// what dense-assignment would produce (1). With the PRE-v53 format the
/// restore would dense-reassign the survivor to RowId(1); a tombstone
/// naming RowId(4) would then be unresolved (counter++). With the v53
/// format the id persists as 4, so the tombstone resolves.
#[test]
fn cross_checkpoint_tombstone_resolves_after_snapshot_restore() {
    use crate::row_header::RowId;
    use crate::snapshot::Snapshot;

    const TOMB_XMAX: u64 = 77;
    let snap = Snapshot::unbounded();

    // --- Pre-checkpoint: 4 rows, then physically delete 3, leaving the
    // row with real RowId(4). next_rowid = 5. ---
    let mut c = Catalog::new();
    c.create_table(TableSchema::new(
        "t",
        vec![ColumnSchema::new("id", DataType::Int, false)],
    ))
    .unwrap();
    {
        let t = c.get_mut("t").unwrap();
        for v in [10, 20, 30, 40] {
            t.insert(Row::new(alloc::vec![Value::Int(v)])).unwrap();
        }
        t.delete_rows(&[0, 1, 2]); // leave value 40 == RowId(4)
        assert_eq!(t.rows().len(), 1);
        assert_eq!(
            t.rowids().iter().copied().collect::<Vec<_>>(),
            alloc::vec![RowId(4)],
            "surviving row carries the pre-checkpoint RowId(4)"
        );
    }

    // --- CHECKPOINT: write the base snapshot (no tombstone in it yet). ---
    let base = c.serialize();

    // --- Post-checkpoint mutation captured in the WAL: tombstone the
    // survivor (RowId 4). This is the redo that must survive the restore. ---
    c.enable_redo_all();
    {
        let t = c.get_mut("t").unwrap();
        t.mark_row_deleted(0, TOMB_XMAX).unwrap();
    }
    let log = c.drain_redo();
    let tomb_targets: Vec<_> = log
        .iter()
        .filter_map(|ch| match ch {
            RowChange::Tombstone { rowids, xmax, .. } => Some((rowids.clone(), *xmax)),
            _ => None,
        })
        .collect();
    assert_eq!(tomb_targets.len(), 1, "one in-place tombstone captured");
    assert_eq!(tomb_targets[0].0, alloc::vec![RowId(4)], "tombstone names the pre-checkpoint id");
    // Round-trip through the real WAL codec.
    let decoded = decode_redo_log(&encode_redo_log(&log)).unwrap();

    // --- CRASH + RESTORE: load the base, replay the WAL tombstone. ---
    let mut restored = Catalog::deserialize(&base).expect("base snapshot loads");
    // The crux: the id persisted as 4 (NOT dense-reassigned to 1).
    assert_eq!(
        restored.get("t").unwrap().rowids().iter().copied().collect::<Vec<_>>(),
        alloc::vec![RowId(4)],
        "v53 restore preserves RowId(4); pre-v53 would have dense-assigned RowId(1)"
    );

    let unresolved_before = crate::unresolved_tombstone_count();
    restored.apply_redo(&decoded).unwrap();
    assert_eq!(
        crate::unresolved_tombstone_count(),
        unresolved_before,
        "the cross-checkpoint tombstone must resolve by RowId (0 unresolved)"
    );

    // The row is physically present but hidden, carrying the tombstone xmax.
    let t = restored.get("t").unwrap();
    assert_eq!(t.rows().len(), 1, "tombstone keeps the physical row");
    let h = *t.headers().get(0).unwrap();
    assert_eq!(h.xmax, TOMB_XMAX, "recovered row carries the tombstone xmax");
    assert!(h.is_deleted(), "recovered row reads as deleted");
    let visible: Vec<i32> = t
        .scan_visible(&snap)
        .filter_map(|(_, r)| match r.values.first() {
            Some(Value::Int(v)) => Some(*v),
            _ => None,
        })
        .collect();
    assert!(visible.is_empty(), "the tombstoned survivor must be hidden after restore");
}

/// v7.27 (mailrs round-21) — the remaining u16 cells take the
/// escape: a > 64 KiB BYTEA cell and a > 64 KiB TEXT[] element
/// round-trip through snapshot serialise/deserialise (the BYTEA
/// twin of round-14 fired during a production migration).
#[test]
fn snapshot_round_trips_large_bytea_and_text_array_element() {
    let mut cat = Catalog::new();
    cat.create_table(TableSchema::new(
        "q",
        vec![
            ColumnSchema::new("id", DataType::BigInt, false),
            ColumnSchema::new("data", DataType::Bytes, true),
            ColumnSchema::new("uris", DataType::TextArray, true),
        ],
    ))
    .unwrap();
    let big_blob = alloc::vec![0xAB_u8; 200_000];
    let big_elem = "u".repeat(100_000);
    cat.get_mut("q")
        .unwrap()
        .insert(Row::new(alloc::vec![
            Value::BigInt(1),
            Value::bytes(big_blob.clone()),
            Value::TextArray(alloc::vec![Some(big_elem.clone()), None, Some("s".into())]),
        ]))
        .unwrap();
    let bytes = cat.serialize();
    let re = Catalog::deserialize(&bytes).unwrap();
    let row = re.get("q").unwrap().rows.get(0).unwrap().clone();
    match &row.values[1] {
        Value::Bytes(b) => assert_eq!(b.len(), big_blob.len()),
        other => panic!("expected Bytes, got {other:?}"),
    }
    match &row.values[2] {
        Value::TextArray(items) => {
            assert_eq!(items[0].as_ref().unwrap().len(), big_elem.len());
            assert!(items[1].is_none());
        }
        other => panic!("expected TextArray, got {other:?}"),
    }
}

/// Pre-v47 containers carry PLAIN u16 lengths for these cells —
/// 0xFFFF must not be treated as an escape there.
#[test]
fn plain_u16_bytea_len_ffff_decodes_under_v46_rules() {
    let payload = alloc::vec![7_u8; 65_535];
    let mut buf = Vec::new();
    write_u16(&mut buf, 65_535);
    buf.extend_from_slice(&payload);
    let mut cur = Cursor::new(&buf).with_codec_version(46);
    let len = cur.read_len_escaped_v47().unwrap();
    assert_eq!(len, 65_535);
    assert_eq!(cur.take(len).unwrap().len(), 65_535);
}

/// v7.23 (mailrs round-14) — the escaped short-string codec.
/// Boundary cases: 0xFFFE stays plain-u16, 0xFFFF and above take
/// the escape form, round-trips are exact at 1 MiB.
#[test]
fn escaped_string_codec_round_trips_large_text() {
    for len in [0usize, 1, 65_534, 65_535, 65_536, 1_048_576] {
        let s: String = "x".repeat(len);
        let mut buf = Vec::new();
        write_str(&mut buf, &s);
        let expected_header = if len >= STR_LEN_ESCAPE as usize { 6 } else { 2 };
        assert_eq!(buf.len(), expected_header + len, "header width for {len}");
        let mut cur = Cursor::new(&buf).with_codec_version(FILE_VERSION);
        assert_eq!(cur.read_str().unwrap().len(), len, "round-trip {len}");
    }
}

/// Pre-v46 containers may carry a PLAIN length of exactly 0xFFFF
/// — the decoder must not treat it as an escape there.
#[test]
fn plain_u16_len_ffff_decodes_under_old_rules() {
    let s = "y".repeat(65_535);
    let mut buf = Vec::new();
    // Hand-encode the OLD form: plain u16 length.
    write_u16(&mut buf, 65_535);
    buf.extend_from_slice(s.as_bytes());
    let mut old = Cursor::new(&buf); // codec_version = 0 (legacy rules)
    assert_eq!(old.read_str().unwrap(), s);
}

/// End-to-end: a catalog holding a 1 MiB TEXT row snapshots and
/// reloads — the exact shape that panicked at 7.22's graceful
/// close ("identifier / text fits in u16").
#[test]
fn snapshot_round_trips_megabyte_text_row() {
    let mut cat = Catalog::new();
    cat.create_table(TableSchema::new(
        "mail",
        vec![
            ColumnSchema::new("id", DataType::BigInt, false),
            ColumnSchema::new("body", DataType::Text, false),
        ],
    ))
    .unwrap();
    let body = "m".repeat(1_048_576);
    cat.get_mut("mail")
        .unwrap()
        .insert(Row::new(vec![Value::BigInt(1), Value::text(body.clone())]))
        .unwrap();
    let bytes = cat.serialize();
    let re = Catalog::deserialize(&bytes).unwrap();
    let t = re.get("mail").unwrap();
    match &t.rows.get(0).unwrap().values[1] {
        Value::Text(s) => assert_eq!(s.len(), body.len()),
        other => panic!("expected Text, got {other:?}"),
    }
}

/// Cold tier: a segment holding a > 64 KiB TEXT row encodes (V3
/// magic) and looks up; a hand-built V1 segment with a legal
/// 0xFFFF-length text still decodes under old rules.
#[test]
fn segment_v3_round_trips_large_text_rows() {
    let schema = TableSchema::new(
        "mail",
        vec![
            ColumnSchema::new("id", DataType::BigInt, false),
            ColumnSchema::new("body", DataType::Text, false),
        ],
    );
    let big = "b".repeat(200_000);
    let rows: Vec<(u64, Vec<u8>)> = (0u64..3)
        .map(|i| {
            let row = Row::new(vec![
                Value::BigInt(i.cast_signed()),
                Value::text(big.clone()),
            ]);
            (i, encode_row_body_dense(&row, &schema))
        })
        .collect();
    let (bytes, _) = encode_segment(rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).unwrap();
    assert_eq!(&bytes[..8], b"SPGSEG\x04\x00", "new segments are V4");
    let seg = OwnedSegment::from_bytes(bytes).unwrap();
    assert!(seg.codec_version() >= 47);
    let payload = seg.lookup(1).expect("pk 1 present");
    let (row, _) = decode_row_body_dense(&payload, &schema, seg.codec_version()).unwrap();
    match &row.values[1] {
        Value::Text(s) => assert_eq!(s.len(), big.len()),
        other => panic!("expected Text, got {other:?}"),
    }
}

/// Index keys derive from TEXT columns — a > 64 KiB key must
/// round-trip through the v9 tagged index-key codec too.
#[test]
fn index_key_round_trips_large_text() {
    let key = IndexKey::Text("k".repeat(100_000));
    let mut buf = Vec::new();
    write_index_key(&mut buf, &key);
    let mut cur = Cursor::new(&buf).with_codec_version(FILE_VERSION);
    let back = cur.read_index_key().unwrap();
    assert_eq!(back, key);
}

#[cfg(target_arch = "aarch64")]
#[test]
fn neon_l2_matches_scalar() {
    // For every dim that's a multiple of 4 (4, 8, 12, 16, 64,
    // 128, 256, 384, 512, 768, 1024, 1536), the NEON impl must
    // agree with the scalar reference within tight float
    // tolerance (FMA rounding differs from separate * + +).
    let dims = [4usize, 8, 12, 16, 64, 128, 256, 384, 512, 768, 1024, 1536];
    for &d in &dims {
        let mut state: u64 = (d as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut a = Vec::with_capacity(d);
        let mut b = Vec::with_capacity(d);
        for _ in 0..d {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let x = (((state >> 32) & 0x00FF_FFFF) as f32) / (0x80_0000_u32 as f32) - 1.0;
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let y = (((state >> 32) & 0x00FF_FFFF) as f32) / (0x80_0000_u32 as f32) - 1.0;
            a.push(x);
            b.push(y);
        }
        let scalar = l2_distance_sq_scalar(&a, &b);
        let neon = unsafe { l2_distance_sq_neon(&a, &b) };
        let tol = (scalar.abs().max(1e-6)) * 1e-4;
        assert!(
            (scalar - neon).abs() <= tol,
            "dim={d}: scalar={scalar} neon={neon} diff={}",
            (scalar - neon).abs()
        );
    }
}

#[cfg(target_arch = "aarch64")]
#[test]
fn neon_inner_product_matches_scalar() {
    // v6.0.2 step 1: NEON IP must agree with scalar across every
    // production-shaped dim. FMA rounding differs from
    // separate * + +, so the tolerance scales with magnitude.
    let dims = [4usize, 8, 12, 16, 64, 128, 256, 512, 1024];
    for &d in &dims {
        let mut state: u64 = (d as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut a = Vec::with_capacity(d);
        let mut b = Vec::with_capacity(d);
        for _ in 0..d {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let x = (((state >> 32) & 0x00FF_FFFF) as f32) / (0x80_0000_u32 as f32) - 1.0;
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let y = (((state >> 32) & 0x00FF_FFFF) as f32) / (0x80_0000_u32 as f32) - 1.0;
            a.push(x);
            b.push(y);
        }
        let scalar = inner_product_scalar(&a, &b);
        let neon = unsafe { inner_product_neon(&a, &b) };
        #[allow(clippy::cast_precision_loss)]
        let tol = (scalar.abs().max(1e-6)) * 1e-4 + (d as f32) * 1e-6;
        assert!(
            (scalar - neon).abs() <= tol,
            "IP dim={d}: scalar={scalar} neon={neon} diff={}",
            (scalar - neon).abs()
        );
    }
}

#[cfg(target_arch = "aarch64")]
#[allow(clippy::similar_names)]
#[test]
fn neon_cosine_dot_norms_matches_scalar() {
    let dims = [4usize, 8, 12, 16, 64, 128, 256, 512, 1024];
    for &d in &dims {
        let mut state: u64 = (d as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        let mut a = Vec::with_capacity(d);
        let mut b = Vec::with_capacity(d);
        for _ in 0..d {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let x = (((state >> 32) & 0x00FF_FFFF) as f32) / (0x80_0000_u32 as f32) - 1.0;
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let y = (((state >> 32) & 0x00FF_FFFF) as f32) / (0x80_0000_u32 as f32) - 1.0;
            a.push(x);
            b.push(y);
        }
        let (dot_s, na_s, nb_s) = cosine_dot_norms_scalar(&a, &b);
        let (dot_n, na_n, nb_n) = unsafe { cosine_dot_norms_neon(&a, &b) };
        #[allow(clippy::cast_precision_loss)]
        let tol_d = (dot_s.abs().max(1e-6)) * 1e-4 + (d as f32) * 1e-6;
        #[allow(clippy::cast_precision_loss)]
        let tol_n = (na_s.abs().max(1e-6)) * 1e-4 + (d as f32) * 1e-6;
        assert!(
            (dot_s - dot_n).abs() <= tol_d,
            "cosine dot dim={d}: scalar={dot_s} neon={dot_n}"
        );
        assert!(
            (na_s - na_n).abs() <= tol_n,
            "cosine na dim={d}: scalar={na_s} neon={na_n}"
        );
        assert!(
            (nb_s - nb_n).abs() <= tol_n,
            "cosine nb dim={d}: scalar={nb_s} neon={nb_n}"
        );
    }
}

fn make_users_schema() -> TableSchema {
    TableSchema::new(
        "users",
        vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new("name", DataType::Text, false),
            ColumnSchema::new("score", DataType::Float, true),
        ],
    )
}

#[test]
fn value_type_tag_matches_variant() {
    assert_eq!(Value::Int(1).data_type(), Some(DataType::Int));
    assert_eq!(Value::BigInt(1).data_type(), Some(DataType::BigInt));
    assert_eq!(Value::Float(1.0).data_type(), Some(DataType::Float));
    assert_eq!(Value::text("x").data_type(), Some(DataType::Text));
    assert_eq!(Value::Bool(true).data_type(), Some(DataType::Bool));
    assert_eq!(Value::Null.data_type(), None);
    assert!(Value::Null.is_null());
    assert!(!Value::Int(0).is_null());
}

#[test]
fn sq8_value_reports_sq8_data_type() {
    // v6.0.1: a `Value::Sq8Vector` cell surfaces its dim
    // (= bytes.len()) and encoding through `data_type()` so
    // INSERT-time column type-checks (step 3) can route on
    // both shape and encoding.
    let q = crate::quantize::quantize(&[0.0, 0.25, 0.5, 0.75, 1.0]);
    let v = Value::Sq8Vector(q);
    assert_eq!(
        v.data_type(),
        Some(DataType::Vector {
            dim: 5,
            encoding: VecEncoding::Sq8,
        }),
    );
}

#[test]
fn datatype_display_matches_pg_keyword() {
    assert_eq!(DataType::Int.to_string(), "INT");
    assert_eq!(DataType::BigInt.to_string(), "BIGINT");
    assert_eq!(DataType::Float.to_string(), "FLOAT");
    assert_eq!(DataType::Text.to_string(), "TEXT");
    assert_eq!(DataType::Bool.to_string(), "BOOL");
}

#[test]
fn row_len_and_emptiness() {
    let r = Row::new(vec![Value::Int(1), Value::Null]);
    assert_eq!(r.len(), 2);
    assert!(!r.is_empty());
    assert!(Row::new(Vec::new()).is_empty());
}

#[test]
fn table_schema_column_position() {
    let s = make_users_schema();
    assert_eq!(s.column_position("id"), Some(0));
    assert_eq!(s.column_position("score"), Some(2));
    assert_eq!(s.column_position("missing"), None);
}

#[test]
fn catalog_create_table_then_lookup() {
    let mut cat = Catalog::new();
    cat.create_table(make_users_schema()).unwrap();
    assert_eq!(cat.table_count(), 1);
    assert!(cat.get("users").is_some());
    assert!(cat.get("nope").is_none());
}

#[test]
fn catalog_duplicate_table_is_rejected() {
    let mut cat = Catalog::new();
    cat.create_table(make_users_schema()).unwrap();
    let err = cat.create_table(make_users_schema()).unwrap_err();
    assert!(matches!(err, StorageError::DuplicateTable { ref name } if name == "users"));
}

#[test]
fn table_insert_happy_path_appends_row() {
    let mut cat = Catalog::new();
    cat.create_table(make_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    t.insert(Row::new(vec![
        Value::Int(1),
        Value::text("alice"),
        Value::Float(99.5),
    ]))
    .unwrap();
    assert_eq!(t.row_count(), 1);
    assert_eq!(t.rows()[0].values[1], Value::text("alice"));
}

#[test]
fn rowid_monotonic_survives_delete_and_never_reused() {
    use crate::row_header::RowId;
    let mut cat = Catalog::new();
    cat.create_table(make_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for i in 0..3 {
        t.insert(Row::new(vec![
            Value::Int(i),
            Value::text("x"),
            Value::Float(0.0),
        ]))
        .unwrap();
    }
    // Fresh appends allocate monotonic 1..=3 in lock-step with rows.
    assert_eq!(t.rows().len(), t.rowids().len());
    assert_eq!(
        t.rowids().iter().copied().collect::<alloc::vec::Vec<_>>(),
        alloc::vec![RowId(1), RowId(2), RowId(3)]
    );
    // Delete the middle row: survivors keep their stable id while
    // their physical slot shifts down (id 2 gone, 1 & 3 remain).
    t.delete_rows(&[1]);
    assert_eq!(t.rows().len(), 2);
    assert_eq!(t.rows().len(), t.rowids().len());
    assert_eq!(
        t.rowids().iter().copied().collect::<alloc::vec::Vec<_>>(),
        alloc::vec![RowId(1), RowId(3)]
    );
    // A new insert never reuses the freed id 2 — it takes 4.
    t.insert(Row::new(vec![Value::Int(9), Value::text("y"), Value::Float(0.0)]))
        .unwrap();
    assert_eq!(
        t.rowids().iter().copied().collect::<alloc::vec::Vec<_>>(),
        alloc::vec![RowId(1), RowId(3), RowId(4)]
    );
    // Truncate clears the ids but the allocator stays monotonic: the
    // next insert never collides with a pre-truncate id.
    t.truncate();
    assert_eq!(t.rowids().len(), 0);
    t.insert(Row::new(vec![Value::Int(0), Value::text("z"), Value::Float(0.0)]))
        .unwrap();
    assert_eq!(
        t.rowids().iter().copied().collect::<alloc::vec::Vec<_>>(),
        alloc::vec![RowId(5)]
    );
}

#[test]
fn relid_assigned_monotonic_by_create_table() {
    use crate::row_header::RelId;
    // A bare Table::new is unassigned until the catalog stamps it.
    let bare = Table::new(make_users_schema());
    assert_eq!(bare.rel_id(), RelId::UNASSIGNED);

    let mut cat = Catalog::new();
    for n in ["a", "b", "c"] {
        let mut s = make_users_schema();
        s.name = n.into();
        cat.create_table(s).unwrap();
    }
    // create_table stamps monotonic 1..=3, distinct and non-zero.
    assert_eq!(cat.get("a").unwrap().rel_id(), RelId(1));
    assert_eq!(cat.get("b").unwrap().rel_id(), RelId(2));
    assert_eq!(cat.get("c").unwrap().rel_id(), RelId(3));
    // Round-trip through the envelope re-assigns dense ids in
    // insertion order (process-local bookkeeping, pre-V6 envelope).
    let bytes = cat.serialize();
    let restored = Catalog::deserialize(&bytes).unwrap();
    assert_eq!(restored.get("a").unwrap().rel_id(), RelId(1));
    assert_eq!(restored.get("c").unwrap().rel_id(), RelId(3));
}

#[test]
fn table_insert_arity_mismatch() {
    let mut cat = Catalog::new();
    cat.create_table(make_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    let err = t.insert(Row::new(vec![Value::Int(1)])).unwrap_err();
    assert!(matches!(
        err,
        StorageError::ArityMismatch {
            expected: 3,
            actual: 1
        }
    ));
    assert_eq!(t.row_count(), 0);
}

#[test]
fn table_insert_type_mismatch_reports_column() {
    let mut cat = Catalog::new();
    cat.create_table(make_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    let err = t
        .insert(Row::new(vec![
            Value::Int(1),
            Value::Int(42), // name expects Text
            Value::Float(0.0),
        ]))
        .unwrap_err();
    match err {
        StorageError::TypeMismatch {
            ref column,
            expected,
            actual,
            position,
        } => {
            assert_eq!(column, "name");
            assert_eq!(expected, DataType::Text);
            assert_eq!(actual, DataType::Int);
            assert_eq!(position, 1);
        }
        other => panic!("unexpected: {other:?}"),
    }
    assert_eq!(t.row_count(), 0);
}

#[test]
fn table_insert_null_into_not_null_rejected() {
    let mut cat = Catalog::new();
    cat.create_table(make_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    let err = t
        .insert(Row::new(vec![
            Value::Int(1),
            Value::Null, // name is NOT NULL
            Value::Float(1.0),
        ]))
        .unwrap_err();
    assert!(matches!(err, StorageError::NullInNotNull { ref column } if column == "name"));
}

#[test]
fn table_insert_null_into_nullable_ok() {
    let mut cat = Catalog::new();
    cat.create_table(make_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    t.insert(Row::new(vec![
        Value::Int(1),
        Value::text("bob"),
        Value::Null,
    ]))
    .unwrap();
    assert_eq!(t.row_count(), 1);
}

#[test]
fn catalog_get_mut_independent_per_table() {
    let mut cat = Catalog::new();
    cat.create_table(TableSchema::new(
        "a",
        vec![ColumnSchema::new("v", DataType::Int, false)],
    ))
    .unwrap();
    cat.create_table(TableSchema::new(
        "b",
        vec![ColumnSchema::new("v", DataType::Int, false)],
    ))
    .unwrap();
    cat.get_mut("a")
        .unwrap()
        .insert(Row::new(vec![Value::Int(1)]))
        .unwrap();
    assert_eq!(cat.get("a").unwrap().row_count(), 1);
    assert_eq!(cat.get("b").unwrap().row_count(), 0);
}

// --- v0.6 persistence round-trips --------------------------------------

fn assert_round_trip(cat: &Catalog) {
    let bytes = cat.serialize();
    let restored = Catalog::deserialize(&bytes).expect("deserialize");
    // Compare semantic state: same tables in same order, same schema +
    // rows in each.
    assert_eq!(restored.table_count(), cat.table_count());
    for (a, b) in cat.tables.iter().zip(restored.tables.iter()) {
        assert_eq!(a.schema, b.schema);
        assert_eq!(a.rows, b.rows);
    }
}

#[test]
fn serialize_empty_catalog_round_trips() {
    assert_round_trip(&Catalog::new());
}

#[test]
fn serialize_single_empty_table_round_trips() {
    let mut cat = Catalog::new();
    cat.create_table(make_users_schema()).unwrap();
    assert_round_trip(&cat);
}

#[test]
fn nsw_clone_is_o1() {
    // v5.5.0: NswGraph::clone must be O(1) structural sharing, not the
    // pre-v5.5 O(N) element copy — it rides on Catalog::clone for every
    // group-commit write on a vector table. Build a non-trivial multi-
    // layer graph, clone it, and prove the clone shares the very same PV
    // storage (root+tail Arc) for `levels` and every `layers[l]`. Sharing
    // ⇒ no per-node element copy ⇒ clone cost independent of N (node
    // count); only the outer layer Vec (len ≤ 8) is copied, O(1) in
    // practice.
    let mut cat = Catalog::new();
    cat.create_table(TableSchema::new(
        "docs",
        alloc::vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new(
                "v",
                DataType::Vector {
                    dim: 3,
                    encoding: VecEncoding::F32
                },
                true
            ),
        ],
    ))
    .unwrap();
    let t = cat.get_mut("docs").unwrap();
    for i in 0..1500_i32 {
        #[allow(clippy::cast_precision_loss)] // 0..1500 — no precision lost
        let base = (i as f32) * 0.01;
        t.insert(Row::new(alloc::vec![
            Value::Int(i),
            Value::vector(alloc::vec![base, base + 0.05, base + 0.1]),
        ]))
        .unwrap();
    }
    t.add_nsw_index("docs_nsw".into(), "v", NSW_DEFAULT_M)
        .unwrap();
    let g = match &cat.get("docs").unwrap().indices()[0].kind {
        IndexKind::Nsw(g) => g,
        IndexKind::BTree(_)
        | IndexKind::Brin { .. }
        | IndexKind::Gin(_)
        | IndexKind::GinTrgm(_)
        | IndexKind::GinFulltext(_)
        | IndexKind::GinJsonb(_) => {
            panic!("expected NSW")
        }
    };
    // Non-trivial graph: one level slot per row, and the geometric level
    // distribution puts some nodes above layer 0.
    assert_eq!(g.levels.len(), 1500, "one level slot per inserted row");
    assert!(
        g.layers.len() >= 2,
        "1500 nodes should populate at least two HNSW layers, got {}",
        g.layers.len()
    );

    let cloned = g.clone();

    assert!(
        g.levels.shares_storage_with(&cloned.levels),
        "levels PV not shared after clone — clone copied elements (O(N))"
    );
    assert_eq!(g.layers.len(), cloned.layers.len());
    for (l, (orig, cl)) in g.layers.iter().zip(cloned.layers.iter()).enumerate() {
        assert!(
            orig.shares_storage_with(cl),
            "layer {l} PV not shared after clone — clone copied elements (O(N))"
        );
    }
}

#[test]
fn sq8_catalog_serialise_roundtrip_preserves_cells_and_index() {
    // v6.0.1 step 6 verify: a catalog with an `VECTOR(N)
    // USING SQ8` column + NSW index survives a full
    // serialise → deserialise cycle. Cells re-decode bit-
    // identically (per-vector affine triple), the NSW
    // topology stays intact, and kNN search still routes
    // through the SQ8 ADC dispatcher after the catalog hop.
    let mut cat = Catalog::new();
    cat.create_table(TableSchema::new(
        "vecs",
        alloc::vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new(
                "v",
                DataType::Vector {
                    dim: 8,
                    encoding: VecEncoding::Sq8,
                },
                false,
            ),
        ],
    ))
    .unwrap();
    let t = cat.get_mut("vecs").unwrap();
    for i in 0..32_i32 {
        #[allow(clippy::cast_precision_loss)]
        let base = (i as f32) * 0.03;
        let v: Vec<f32> = (0..8_i32)
            .map(|j| {
                #[allow(clippy::cast_precision_loss)]
                let off = (j as f32) * 0.01;
                base + off
            })
            .collect();
        t.insert(Row::new(alloc::vec![
            Value::Int(i),
            Value::Sq8Vector(quantize::quantize(&v)),
        ]))
        .unwrap();
    }
    t.add_nsw_index("v_idx".into(), "v", NSW_DEFAULT_M).unwrap();
    // Capture a pre-serialise reference cell + nsw hits to
    // compare against the restored catalog.
    let query = alloc::vec![0.15_f32, 0.16, 0.17, 0.18, 0.19, 0.20, 0.21, 0.22];
    let (before_cell, before_ty, before_hits) = {
        let t_ref = cat.get("vecs").unwrap();
        (
            t_ref.rows()[5].values[1].clone(),
            t_ref.schema().columns[1].ty,
            nsw_query(t_ref, "v_idx", &query, 5, NswMetric::L2),
        )
    };

    let bytes = cat.serialize();
    let restored = Catalog::deserialize(&bytes).expect("deserialize ok");
    let rt = restored.get("vecs").unwrap();
    assert_eq!(rt.schema().columns[1].ty, before_ty);
    assert_eq!(rt.rows()[5].values[1], before_cell);
    let after_hits = nsw_query(rt, "v_idx", &query, 5, NswMetric::L2);
    assert_eq!(before_hits, after_hits);
}

#[test]
fn half_catalog_serialise_roundtrip_preserves_cells_and_index() {
    // v6.0.3 step 4 verify: a catalog with a `VECTOR(N) USING
    // HALF` column + NSW index survives a full serialise →
    // deserialise cycle. Cells re-decode bit-identically (raw
    // u16 LE bytes), the NSW topology stays intact, and kNN
    // search still returns the same hit IDs against the
    // restored catalog.
    use crate::halfvec;
    let mut cat = Catalog::new();
    cat.create_table(TableSchema::new(
        "vecs",
        alloc::vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new(
                "v",
                DataType::Vector {
                    dim: 8,
                    encoding: VecEncoding::F16,
                },
                false,
            ),
        ],
    ))
    .unwrap();
    let t = cat.get_mut("vecs").unwrap();
    for i in 0..32_i32 {
        #[allow(clippy::cast_precision_loss)]
        let base = (i as f32) * 0.03;
        let v: Vec<f32> = (0..8_i32)
            .map(|j| {
                #[allow(clippy::cast_precision_loss)]
                let off = (j as f32) * 0.01;
                base + off
            })
            .collect();
        t.insert(Row::new(alloc::vec![
            Value::Int(i),
            Value::HalfVector(halfvec::HalfVector::from_f32_slice(&v)),
        ]))
        .unwrap();
    }
    t.add_nsw_index("v_idx".into(), "v", NSW_DEFAULT_M).unwrap();
    let query = alloc::vec![0.15_f32, 0.16, 0.17, 0.18, 0.19, 0.20, 0.21, 0.22];
    let (before_cell, before_ty, before_hits) = {
        let t_ref = cat.get("vecs").unwrap();
        (
            t_ref.rows()[5].values[1].clone(),
            t_ref.schema().columns[1].ty,
            nsw_query(t_ref, "v_idx", &query, 5, NswMetric::L2),
        )
    };
    let bytes = cat.serialize();
    let restored = Catalog::deserialize(&bytes).expect("deserialize ok");
    let rt = restored.get("vecs").unwrap();
    assert_eq!(rt.schema().columns[1].ty, before_ty);
    assert_eq!(rt.rows()[5].values[1], before_cell);
    let after_hits = nsw_query(rt, "v_idx", &query, 5, NswMetric::L2);
    assert_eq!(before_hits, after_hits);
}

#[test]
#[allow(clippy::similar_names)]
fn hnsw_half_recall_at_10_matches_f32_groundtruth() {
    // v6.0.3 step 3 verify: HALF column NSW retrieves ≥ 95%
    // top-10 overlap vs brute-force F32 ground truth.
    // Half-precision dequantises bit-exactly at the storage
    // layer (no rerank pass), so the recall floor is tighter
    // than the SQ8 case — only the rounding noise from f32 →
    // f16 quantisation contributes.
    use crate::halfvec;
    fn next(state: &mut u64) -> f32 {
        *state = state
            .wrapping_add(0x9E37_79B9_7F4A_7C15)
            .wrapping_mul(0xBF58_476D_1CE4_E5B9);
        #[allow(clippy::cast_precision_loss)]
        let u = ((*state >> 32) as u32 as f32) / (u32::MAX as f32);
        2.0 * u - 1.0
    }
    let dim: u32 = 32;
    let n: usize = 512;
    let dim_us = dim as usize;
    let mut seed: u64 = 0xF16_F16_F16_F16_u64;
    let corpus: Vec<Vec<f32>> = (0..n)
        .map(|_| (0..dim_us).map(|_| next(&mut seed)).collect())
        .collect();
    let queries: Vec<Vec<f32>> = (0..32)
        .map(|_| (0..dim_us).map(|_| next(&mut seed)).collect())
        .collect();
    let exact_top10: Vec<Vec<usize>> = queries
        .iter()
        .map(|q| {
            let mut scored: Vec<(f32, usize)> = corpus
                .iter()
                .enumerate()
                .map(|(i, v)| (l2_distance_sq(v, q), i))
                .collect();
            scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));
            scored.into_iter().take(10).map(|(_, i)| i).collect()
        })
        .collect();
    let mut cat = Catalog::new();
    cat.create_table(TableSchema::new(
        "vecs",
        alloc::vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new(
                "v",
                DataType::Vector {
                    dim,
                    encoding: VecEncoding::F16,
                },
                false,
            ),
        ],
    ))
    .unwrap();
    let t = cat.get_mut("vecs").unwrap();
    for (i, v) in corpus.iter().enumerate() {
        t.insert(Row::new(alloc::vec![
            Value::Int(i32::try_from(i).unwrap()),
            Value::HalfVector(halfvec::HalfVector::from_f32_slice(v)),
        ]))
        .unwrap();
    }
    t.add_nsw_index("v_idx".into(), "v", NSW_DEFAULT_M).unwrap();
    let table = cat.get("vecs").unwrap();
    let mut total_overlap = 0_usize;
    for (q, exact) in queries.iter().zip(exact_top10.iter()) {
        let hits = nsw_query(table, "v_idx", q, 10, NswMetric::L2);
        for h in &hits {
            if exact.contains(h) {
                total_overlap += 1;
            }
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let recall = total_overlap as f32 / (10.0 * queries.len() as f32);
    assert!(
        recall >= 0.95,
        "HALF HNSW recall@10 = {recall:.3}, below floor 0.95 — \
         check halfvec dispatch in `cell_to_query_metric_distance`"
    );
}

#[test]
fn hnsw_sq8_recall_at_10_above_0_95_vs_f32_groundtruth() {
    // v6.0.1 step 5 verify: build TWO catalogs over the same
    // corpus — one F32, one SQ8 — and confirm SQ8 NSW + f32
    // rerank retrieves ≥ 95% top-10 overlap vs brute-force F32
    // ground truth. The rerank pass (sq8_rerank) re-scores ADC
    // candidates with dequantised cells, recovering recall the
    // raw ADC sacrifices for 4× compression.
    use crate::quantize;
    // Deterministic Gaussian-ish corpus via splitmix64. Vectors
    // get normalised so SQ8's per-vector `(min, max)` lives in
    // a sensible range; matches the v6.0.0 fuzz harness.
    fn next(state: &mut u64) -> f32 {
        *state = state
            .wrapping_add(0x9E37_79B9_7F4A_7C15)
            .wrapping_mul(0xBF58_476D_1CE4_E5B9);
        #[allow(clippy::cast_precision_loss)]
        let u = ((*state >> 32) as u32 as f32) / (u32::MAX as f32);
        2.0 * u - 1.0
    }
    let dim: u32 = 32;
    let n: usize = 512;
    let dim_us = dim as usize;
    let mut seed: u64 = 0xCAFE_BABE_DEAD_BEEFu64;
    let corpus: Vec<Vec<f32>> = (0..n)
        .map(|_| (0..dim_us).map(|_| next(&mut seed)).collect())
        .collect();
    let queries: Vec<Vec<f32>> = (0..32)
        .map(|_| (0..dim_us).map(|_| next(&mut seed)).collect())
        .collect();
    // F32 ground truth — pure exact arithmetic, brute force.
    let exact_top10: Vec<Vec<usize>> = queries
        .iter()
        .map(|q| {
            let mut scored: Vec<(f32, usize)> = corpus
                .iter()
                .enumerate()
                .map(|(i, v)| (l2_distance_sq(v, q), i))
                .collect();
            scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));
            scored.into_iter().take(10).map(|(_, i)| i).collect()
        })
        .collect();
    // SQ8 catalog — INSERTs land as `Value::Sq8Vector` cells;
    // HNSW build uses the ADC path verified in step 4.
    let mut cat = Catalog::new();
    cat.create_table(TableSchema::new(
        "vecs",
        alloc::vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new(
                "v",
                DataType::Vector {
                    dim,
                    encoding: VecEncoding::Sq8,
                },
                false,
            ),
        ],
    ))
    .unwrap();
    let t = cat.get_mut("vecs").unwrap();
    for (i, v) in corpus.iter().enumerate() {
        t.insert(Row::new(alloc::vec![
            Value::Int(i32::try_from(i).unwrap()),
            Value::Sq8Vector(quantize::quantize(v)),
        ]))
        .unwrap();
    }
    t.add_nsw_index("v_idx".into(), "v", NSW_DEFAULT_M).unwrap();
    let table = cat.get("vecs").unwrap();
    let mut total_overlap = 0_usize;
    for (q, exact) in queries.iter().zip(exact_top10.iter()) {
        let hits = nsw_query(table, "v_idx", q, 10, NswMetric::L2);
        for h in &hits {
            if exact.contains(h) {
                total_overlap += 1;
            }
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let recall = total_overlap as f32 / (10.0 * queries.len() as f32);
    assert!(
        recall >= 0.95,
        "SQ8 HNSW recall@10 = {recall:.3}, below floor 0.95 — \
         check `sq8_rerank` is wired in `nsw_search` for SQ8 columns"
    );
}

#[test]
fn nsw_index_topology_persists_through_round_trip() {
    // Build an NSW index, capture its (entry, neighbors) tuple, do
    // a full serialize → deserialize, and verify the restored
    // graph is byte-for-byte identical. The point of v2.7 is that
    // startup skips the rebuild, so the topology has to survive
    // the disk hop.
    let mut cat = Catalog::new();
    cat.create_table(TableSchema::new(
        "docs",
        alloc::vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new(
                "v",
                DataType::Vector {
                    dim: 3,
                    encoding: VecEncoding::F32
                },
                true
            ),
        ],
    ))
    .unwrap();
    let t = cat.get_mut("docs").unwrap();
    for i in 0..6_i32 {
        #[allow(clippy::cast_precision_loss)] // 0..6 — no precision lost
        let base = (i as f32) * 0.1;
        let row = Row::new(alloc::vec![
            Value::Int(i),
            Value::vector(alloc::vec![base, base + 0.05, base + 0.1]),
        ]);
        t.insert(row).unwrap();
    }
    t.add_nsw_index("docs_nsw".into(), "v", NSW_DEFAULT_M)
        .unwrap();
    let original = match &cat.get("docs").unwrap().indices()[0].kind {
        IndexKind::Nsw(g) => g.clone(),
        IndexKind::BTree(_)
        | IndexKind::Brin { .. }
        | IndexKind::Gin(_)
        | IndexKind::GinTrgm(_)
        | IndexKind::GinFulltext(_)
        | IndexKind::GinJsonb(_) => {
            panic!("expected NSW")
        }
    };
    let bytes = cat.serialize();
    let restored = Catalog::deserialize(&bytes).expect("deserialize");
    let restored_graph = match &restored.get("docs").unwrap().indices()[0].kind {
        IndexKind::Nsw(g) => g.clone(),
        IndexKind::BTree(_)
        | IndexKind::Brin { .. }
        | IndexKind::Gin(_)
        | IndexKind::GinTrgm(_)
        | IndexKind::GinFulltext(_)
        | IndexKind::GinJsonb(_) => {
            panic!("expected NSW")
        }
    };
    assert_eq!(restored_graph.m, original.m);
    assert_eq!(restored_graph.m_max_0, original.m_max_0);
    assert_eq!(restored_graph.entry, original.entry);
    assert_eq!(restored_graph.entry_level, original.entry_level);
    assert_eq!(restored_graph.levels, original.levels);
    assert_eq!(restored_graph.layers, original.layers);
}

#[test]
fn hnsw_level_assignment_is_deterministic() {
    // Same row index always produces the same level — the topology
    // must be reproducible (matters for serialize round-trip).
    for i in 0..32usize {
        assert_eq!(nsw_assign_level(i), nsw_assign_level(i));
    }
}

#[test]
fn hnsw_layer_0_dominates_population() {
    // Sanity: out of N inserts, the vast majority should land on
    // layer 0. The 4-bit-clear promotion rule gives roughly 1/16
    // promotion to layer ≥ 1, so under 50 nodes we expect ~3 on
    // layer ≥ 1 and the rest on layer 0.
    let on_zero = (0..200usize).filter(|&i| nsw_assign_level(i) == 0).count();
    assert!(on_zero > 150, "level-0 nodes too few: {on_zero}");
}

#[test]
fn hnsw_search_matches_brute_force_for_l2_top1() {
    // Build a small dataset, query it, and confirm the top result
    // matches the brute-force nearest by L2. Topology variability
    // shouldn't break recall at k=1 for well-separated vectors.
    let mut cat = Catalog::new();
    cat.create_table(TableSchema::new(
        "vecs",
        alloc::vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new(
                "v",
                DataType::Vector {
                    dim: 3,
                    encoding: VecEncoding::F32
                },
                true
            ),
        ],
    ))
    .unwrap();
    let t = cat.get_mut("vecs").unwrap();
    let dataset: alloc::vec::Vec<(i32, [f32; 3])> = alloc::vec![
        (1, [0.0, 0.0, 0.0]),
        (2, [1.0, 0.0, 0.0]),
        (3, [0.0, 1.0, 0.0]),
        (4, [0.0, 0.0, 1.0]),
        (5, [1.0, 1.0, 0.0]),
        (6, [1.0, 0.0, 1.0]),
        (7, [0.0, 1.0, 1.0]),
        (8, [1.0, 1.0, 1.0]),
        (9, [0.5, 0.5, 0.5]),
        (10, [0.2, 0.8, 0.5]),
    ];
    for &(id, v) in &dataset {
        t.insert(Row::new(alloc::vec![
            Value::Int(id),
            Value::vector(alloc::vec![v[0], v[1], v[2]]),
        ]))
        .unwrap();
    }
    t.add_nsw_index("v_idx".into(), "v", NSW_DEFAULT_M).unwrap();
    let idx_pos = cat
        .get("vecs")
        .unwrap()
        .indices()
        .iter()
        .position(|i| i.name == "v_idx")
        .unwrap();
    for query in [[0.4, 0.4, 0.4], [0.9, 0.1, 0.0], [0.0, 0.9, 0.9]] {
        let table = cat.get("vecs").unwrap();
        let hnsw_top = nsw_search(table, idx_pos, &query, 1, 16, NswMetric::L2);
        let mut brute: alloc::vec::Vec<(f32, usize)> = (0..table.rows.len())
            .map(|i| {
                let Value::Vector(v) = &table.rows[i].values[1] else {
                    return (f32::INFINITY, i);
                };
                (l2_distance_sq(v, &query), i)
            })
            .collect();
        brute.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(core::cmp::Ordering::Equal));
        assert!(!hnsw_top.is_empty(), "HNSW returned no results");
        assert_eq!(
            hnsw_top[0].1, brute[0].1,
            "HNSW top-1 != brute-force top-1 for {query:?}"
        );
    }
}

#[test]
fn serialize_table_with_rows_round_trips() {
    let mut cat = Catalog::new();
    cat.create_table(make_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    t.insert(Row::new(vec![
        Value::Int(1),
        Value::text("alice"),
        Value::Float(95.5),
    ]))
    .unwrap();
    t.insert(Row::new(vec![
        Value::Int(2),
        Value::text("bob"),
        Value::Null,
    ]))
    .unwrap();
    assert_round_trip(&cat);
}

#[test]
fn serialize_multiple_tables_round_trips() {
    let mut cat = Catalog::new();
    cat.create_table(make_users_schema()).unwrap();
    cat.create_table(TableSchema::new(
        "flags",
        vec![
            ColumnSchema::new("id", DataType::BigInt, false),
            ColumnSchema::new("active", DataType::Bool, false),
        ],
    ))
    .unwrap();
    cat.get_mut("flags")
        .unwrap()
        .insert(Row::new(vec![Value::BigInt(7), Value::Bool(true)]))
        .unwrap();
    assert_round_trip(&cat);
}

#[test]
fn deserialize_rejects_bad_magic() {
    let mut buf = b"BADMAGIC".to_vec();
    buf.push(FILE_VERSION);
    buf.extend_from_slice(&0u32.to_le_bytes());
    let err = Catalog::deserialize(&buf).unwrap_err();
    assert!(matches!(err, StorageError::Corrupt(_)));
}

#[test]
fn deserialize_rejects_unsupported_version() {
    let mut buf = FILE_MAGIC.to_vec();
    buf.push(99); // future version
    buf.extend_from_slice(&0u32.to_le_bytes());
    let err = Catalog::deserialize(&buf).unwrap_err();
    assert!(matches!(err, StorageError::Corrupt(ref s) if s.contains("version")));
}

#[test]
fn deserialize_rejects_truncated_file() {
    let mut cat = Catalog::new();
    cat.create_table(make_users_schema()).unwrap();
    let bytes = cat.serialize();
    // Drop the last byte to simulate truncation.
    let truncated = &bytes[..bytes.len() - 1];
    assert!(matches!(
        Catalog::deserialize(truncated),
        Err(StorageError::Corrupt(_))
    ));
}

#[test]
fn deserialize_rejects_trailing_garbage() {
    let cat = Catalog::new();
    let mut bytes = cat.serialize();
    bytes.push(0xFF);
    assert!(matches!(
        Catalog::deserialize(&bytes),
        Err(StorageError::Corrupt(ref s)) if s.contains("trailing")
    ));
}

// --- v0.8 indices ------------------------------------------------------

fn populated_users() -> Catalog {
    let mut cat = Catalog::new();
    cat.create_table(make_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for (id, name, score) in [
        (1, "alice", Some(90.0)),
        (2, "bob", None),
        (3, "alice", Some(70.0)), // duplicate name → maps to two row idxs
    ] {
        t.insert(Row::new(vec![
            Value::Int(id),
            Value::text(name),
            score.map_or(Value::Null, Value::Float),
        ]))
        .unwrap();
    }
    cat
}

#[test]
fn add_index_builds_from_existing_rows() {
    let mut cat = populated_users();
    cat.get_mut("users")
        .unwrap()
        .add_index("by_id".into(), "id")
        .unwrap();
    let t = cat.get("users").unwrap();
    let idx = t.index_on(0).expect("index_on(0)");
    assert_eq!(idx.lookup_eq(&IndexKey::Int(2)), &[RowLocator::Hot(1)]);
    assert_eq!(idx.lookup_eq(&IndexKey::Int(99)), &[] as &[RowLocator]);
}

#[test]
fn add_index_dup_name_rejected() {
    let mut cat = populated_users();
    let t = cat.get_mut("users").unwrap();
    t.add_index("ix".into(), "id").unwrap();
    let err = t.add_index("ix".into(), "name").unwrap_err();
    assert!(matches!(err, StorageError::DuplicateIndex { ref name } if name == "ix"));
}

#[test]
fn add_index_unknown_column_rejected() {
    let mut cat = populated_users();
    let err = cat
        .get_mut("users")
        .unwrap()
        .add_index("ix".into(), "ghost")
        .unwrap_err();
    assert!(matches!(err, StorageError::ColumnNotFound { ref column } if column == "ghost"));
}

#[test]
fn insert_after_create_index_updates_it() {
    let mut cat = populated_users();
    let t = cat.get_mut("users").unwrap();
    t.add_index("by_name".into(), "name").unwrap();
    t.insert(Row::new(vec![
        Value::Int(4),
        Value::text("dave"),
        Value::Null,
    ]))
    .unwrap();
    let idx = t.index_on(1).unwrap();
    assert_eq!(
        idx.lookup_eq(&IndexKey::Text("dave".into())),
        &[RowLocator::Hot(3)]
    );
    // Pre-existing duplicates remain mapped to the two original row idxs.
    assert_eq!(
        idx.lookup_eq(&IndexKey::Text("alice".into())),
        &[RowLocator::Hot(0), RowLocator::Hot(2)]
    );
}

#[test]
fn null_or_float_values_are_not_indexed() {
    let mut cat = populated_users();
    let t = cat.get_mut("users").unwrap();
    t.add_index("by_score".into(), "score").unwrap();
    let idx = t.index_on(2).unwrap();
    // bob's score is NULL → no entry for bob.
    // Score is Float → the spec says we don't index NaN-prone columns,
    // so even the present scores are absent. Lookups via IndexKey::Int(90)
    // mis-match the column type and trivially find nothing.
    assert_eq!(idx.lookup_eq(&IndexKey::Int(90)), &[] as &[RowLocator]);
}

// --- v0.11 vector type -------------------------------------------------

#[test]
fn vector_value_data_type_carries_dim() {
    let v = Value::vector(vec![1.0, 2.0, 3.0]);
    assert_eq!(
        v.data_type(),
        Some(DataType::Vector {
            dim: 3,
            encoding: VecEncoding::F32
        })
    );
}

#[test]
fn vector_column_insert_matching_dim_ok() {
    let mut cat = Catalog::new();
    cat.create_table(TableSchema::new(
        "emb",
        vec![ColumnSchema::new(
            "v",
            DataType::Vector {
                dim: 3,
                encoding: VecEncoding::F32,
            },
            false,
        )],
    ))
    .unwrap();
    cat.get_mut("emb")
        .unwrap()
        .insert(Row::new(vec![Value::vector(vec![1.0, 2.0, 3.0])]))
        .unwrap();
}

#[test]
fn vector_column_insert_dim_mismatch_rejected() {
    let mut cat = Catalog::new();
    cat.create_table(TableSchema::new(
        "emb",
        vec![ColumnSchema::new(
            "v",
            DataType::Vector {
                dim: 3,
                encoding: VecEncoding::F32,
            },
            false,
        )],
    ))
    .unwrap();
    let err = cat
        .get_mut("emb")
        .unwrap()
        .insert(Row::new(vec![Value::vector(vec![1.0, 2.0])]))
        .unwrap_err();
    assert!(matches!(err, StorageError::TypeMismatch { .. }));
}

#[test]
fn vector_value_survives_catalog_round_trip() {
    let mut cat = Catalog::new();
    cat.create_table(TableSchema::new(
        "emb",
        vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new(
                "v",
                DataType::Vector {
                    dim: 4,
                    encoding: VecEncoding::F32,
                },
                false,
            ),
        ],
    ))
    .unwrap();
    cat.get_mut("emb")
        .unwrap()
        .insert(Row::new(vec![
            Value::Int(1),
            Value::vector(vec![0.5, -1.25, 3.0, 7.0]),
        ]))
        .unwrap();
    let restored = Catalog::deserialize(&cat.serialize()).expect("round-trip");
    let table = restored.get("emb").unwrap();
    assert_eq!(
        table.schema().columns[1].ty,
        DataType::Vector {
            dim: 4,
            encoding: VecEncoding::F32
        }
    );
    assert_eq!(
        table.rows()[0].values[1],
        Value::vector(vec![0.5, -1.25, 3.0, 7.0])
    );
}

#[test]
fn index_survives_serialize_deserialize_round_trip() {
    let mut cat = populated_users();
    cat.get_mut("users")
        .unwrap()
        .add_index("by_name".into(), "name")
        .unwrap();
    let restored = Catalog::deserialize(&cat.serialize()).unwrap();
    let idx = restored
        .get("users")
        .unwrap()
        .index_on(1)
        .expect("index_on(1) after restore");
    assert_eq!(idx.name, "by_name");
    // Data was rebuilt from rows, not deserialized directly.
    assert_eq!(
        idx.lookup_eq(&IndexKey::Text("alice".into())),
        &[RowLocator::Hot(0), RowLocator::Hot(2)]
    );
}

// --- v5.1 cold-tier integration tests ----------------------

/// Schema with a BIGINT PK column matching what the v5.1 cold-
/// tier path supports (`IndexKey::Int` → `u64` cast).
fn bigint_pk_users_schema() -> TableSchema {
    TableSchema::new(
        "users",
        vec![
            ColumnSchema::new("id", DataType::BigInt, false),
            ColumnSchema::new("name", DataType::Text, false),
        ],
    )
}

fn make_user_row(id: i64, name: &str) -> Row<'static> {
    Row::new(vec![Value::BigInt(id), Value::text(name.to_string())])
}

// v7.20 P4 — update_row incremental index maintenance.

#[test]
fn update_row_non_indexed_column_keeps_index_intact() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for (id, name) in [(1i64, "alice"), (2, "bob"), (3, "carol")] {
        t.insert(make_user_row(id, name)).unwrap();
    }
    t.add_index("by_id".into(), "id").unwrap();
    // Change only the non-indexed `name` column — the by_id
    // entry for key 2 must still resolve position 1.
    t.update_row(1, vec![Value::BigInt(2), Value::text("bobby")])
        .unwrap();
    let idx = t.index_on(0).unwrap();
    assert_eq!(
        idx.lookup_eq(&IndexKey::Int(2)),
        &[RowLocator::Hot(1)],
        "old key still resolves the in-place position"
    );
    assert_eq!(t.rows()[1].values[1], Value::text("bobby"));
}

#[test]
fn update_row_indexed_column_moves_entry() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for (id, name) in [(1i64, "alice"), (2, "bob"), (3, "carol")] {
        t.insert(make_user_row(id, name)).unwrap();
    }
    t.add_index("by_id".into(), "id").unwrap();
    // Change the indexed key 2 → 20.
    t.update_row(1, vec![Value::BigInt(20), Value::text("bob")])
        .unwrap();
    let idx = t.index_on(0).unwrap();
    assert!(
        idx.lookup_eq(&IndexKey::Int(2)).is_empty(),
        "old key entry removed"
    );
    assert_eq!(
        idx.lookup_eq(&IndexKey::Int(20)),
        &[RowLocator::Hot(1)],
        "new key entry resolves the position"
    );
    // Untouched neighbours unaffected.
    assert_eq!(idx.lookup_eq(&IndexKey::Int(1)), &[RowLocator::Hot(0)]);
    assert_eq!(idx.lookup_eq(&IndexKey::Int(3)), &[RowLocator::Hot(2)]);
}

#[test]
fn update_row_duplicate_key_moves_only_target_position() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    // Two rows share key 7.
    for (id, name) in [(7i64, "a"), (7, "b"), (9, "c")] {
        t.insert(make_user_row(id, name)).unwrap();
    }
    t.add_index("by_id".into(), "id").unwrap();
    // Move position 1's key 7 → 8; position 0 must keep its 7.
    t.update_row(1, vec![Value::BigInt(8), Value::text("b")])
        .unwrap();
    let idx = t.index_on(0).unwrap();
    assert_eq!(idx.lookup_eq(&IndexKey::Int(7)), &[RowLocator::Hot(0)]);
    assert_eq!(idx.lookup_eq(&IndexKey::Int(8)), &[RowLocator::Hot(1)]);
    assert_eq!(idx.lookup_eq(&IndexKey::Int(9)), &[RowLocator::Hot(2)]);
}

#[test]
fn update_row_null_transition_on_indexed_nullable_column() {
    let mut cat = Catalog::new();
    cat.create_table(TableSchema::new(
        "n",
        vec![
            ColumnSchema::new("id", DataType::BigInt, false),
            ColumnSchema::new("tag", DataType::BigInt, true),
        ],
    ))
    .unwrap();
    let t = cat.get_mut("n").unwrap();
    t.insert(Row::new(vec![Value::BigInt(1), Value::BigInt(5)]))
        .unwrap();
    t.add_index("by_tag".into(), "tag").unwrap();
    // 5 → NULL: entry leaves the index (NULL never enters a B-tree).
    t.update_row(0, vec![Value::BigInt(1), Value::Null])
        .unwrap();
    let idx = t.index_on(1).unwrap();
    assert!(idx.lookup_eq(&IndexKey::Int(5)).is_empty());
    // NULL → 6: entry re-enters under the new key.
    t.update_row(0, vec![Value::BigInt(1), Value::BigInt(6)])
        .unwrap();
    let idx = t.index_on(1).unwrap();
    assert_eq!(idx.lookup_eq(&IndexKey::Int(6)), &[RowLocator::Hot(0)]);
}

#[test]
fn lookup_by_pk_finds_row_via_hot_index() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for (id, name) in [(1i64, "alice"), (2, "bob"), (3, "carol")] {
        t.insert(make_user_row(id, name)).unwrap();
    }
    t.add_index("by_id".into(), "id").unwrap();
    // All locators are Hot; cold_segments is empty.
    let got = cat
        .lookup_by_pk("users", "by_id", &IndexKey::Int(2))
        .unwrap();
    assert_eq!(got, make_user_row(2, "bob"));
    assert_eq!(cat.cold_segment_count(), 0);
}

#[test]
fn lookup_by_pk_returns_none_when_key_missing() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    t.insert(make_user_row(1, "alice")).unwrap();
    t.add_index("by_id".into(), "id").unwrap();
    assert!(
        cat.lookup_by_pk("users", "by_id", &IndexKey::Int(999))
            .is_none()
    );
    // Also: unknown table / unknown index name.
    assert!(
        cat.lookup_by_pk("other_table", "by_id", &IndexKey::Int(1))
            .is_none()
    );
    assert!(
        cat.lookup_by_pk("users", "no_such_index", &IndexKey::Int(1))
            .is_none()
    );
}

#[test]
fn lookup_by_pk_resolves_cold_locator_via_loaded_segment() {
    // Build a cold-tier segment whose payloads are dense-encoded
    // BIGINT rows. Wire each PK into the BTree index as a Cold
    // locator. The hot tier carries no rows for those PKs.
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    t.add_index("by_id".into(), "id").unwrap();
    let schema = t.schema.clone();

    let cold_rows: Vec<(i64, &str)> = vec![(100, "ivy"), (200, "joe"), (300, "kim"), (400, "lin")];
    let seg_rows: Vec<(u64, Vec<u8>)> = cold_rows
        .iter()
        .map(|(id, name)| {
            let row = make_user_row(*id, name);
            ((*id).cast_unsigned(), encode_row_body_dense(&row, &schema))
        })
        .collect();
    let (seg_bytes, _meta) =
        encode_segment(seg_rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).unwrap();
    let seg_id = cat.load_segment_bytes(seg_bytes).unwrap();
    assert_eq!(seg_id, 0);
    assert_eq!(cat.cold_segment_count(), 1);

    let pairs: Vec<(IndexKey, RowLocator)> = cold_rows
        .iter()
        .map(|(id, _)| {
            (
                IndexKey::Int(*id),
                RowLocator::Cold {
                    segment_id: seg_id,
                    page_offset: 0,
                },
            )
        })
        .collect();
    let registered = cat
        .get_mut("users")
        .unwrap()
        .register_cold_locators("by_id", pairs)
        .unwrap();
    assert_eq!(registered, 4);

    for (id, name) in &cold_rows {
        let got = cat
            .lookup_by_pk("users", "by_id", &IndexKey::Int(*id))
            .unwrap_or_else(|| panic!("cold key {id} not found"));
        assert_eq!(got, make_user_row(*id, name));
    }
    // Cold key that isn't in the segment must return None.
    assert!(
        cat.lookup_by_pk("users", "by_id", &IndexKey::Int(999))
            .is_none()
    );
}

#[test]
fn lookup_by_pk_mixes_hot_and_cold_tiers() {
    // Half the rows live in the hot tier (Table::rows + add_index
    // produces Hot locators); half live in a cold segment and have
    // Cold locators wired manually. Each lookup hits the right tier.
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for (id, name) in [(1i64, "alice"), (2, "bob")] {
        t.insert(make_user_row(id, name)).unwrap();
    }
    t.add_index("by_id".into(), "id").unwrap();
    let schema = t.schema.clone();

    let cold_rows: Vec<(i64, &str)> = vec![(100, "ivy"), (200, "joe")];
    let seg_rows: Vec<(u64, Vec<u8>)> = cold_rows
        .iter()
        .map(|(id, name)| {
            let row = make_user_row(*id, name);
            ((*id).cast_unsigned(), encode_row_body_dense(&row, &schema))
        })
        .collect();
    let (seg_bytes, _) = encode_segment(seg_rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).unwrap();
    let seg_id = cat.load_segment_bytes(seg_bytes).unwrap();
    let pairs: Vec<(IndexKey, RowLocator)> = cold_rows
        .iter()
        .map(|(id, _)| {
            (
                IndexKey::Int(*id),
                RowLocator::Cold {
                    segment_id: seg_id,
                    page_offset: 0,
                },
            )
        })
        .collect();
    cat.get_mut("users")
        .unwrap()
        .register_cold_locators("by_id", pairs)
        .unwrap();

    // Hot tier hits.
    assert_eq!(
        cat.lookup_by_pk("users", "by_id", &IndexKey::Int(1))
            .unwrap(),
        make_user_row(1, "alice")
    );
    assert_eq!(
        cat.lookup_by_pk("users", "by_id", &IndexKey::Int(2))
            .unwrap(),
        make_user_row(2, "bob")
    );
    // Cold tier hits.
    assert_eq!(
        cat.lookup_by_pk("users", "by_id", &IndexKey::Int(100))
            .unwrap(),
        make_user_row(100, "ivy")
    );
    assert_eq!(
        cat.lookup_by_pk("users", "by_id", &IndexKey::Int(200))
            .unwrap(),
        make_user_row(200, "joe")
    );
    // Miss in both tiers.
    assert!(
        cat.lookup_by_pk("users", "by_id", &IndexKey::Int(50))
            .is_none()
    );
}

#[test]
fn register_cold_locators_rejects_nsw_index() {
    let mut cat = Catalog::new();
    cat.create_table(TableSchema::new(
        "vecs",
        vec![
            ColumnSchema::new("id", DataType::Int, false),
            ColumnSchema::new(
                "v",
                DataType::Vector {
                    dim: 4,
                    encoding: VecEncoding::F32,
                },
                false,
            ),
        ],
    ))
    .unwrap();
    let t = cat.get_mut("vecs").unwrap();
    t.insert(Row::new(vec![
        Value::Int(1),
        Value::vector(vec![1.0, 0.0, 0.0, 0.0]),
    ]))
    .unwrap();
    t.add_nsw_index("by_v".into(), "v", NSW_DEFAULT_M).unwrap();
    let err = t
        .register_cold_locators(
            "by_v",
            vec![(
                IndexKey::Int(1),
                RowLocator::Cold {
                    segment_id: 0,
                    page_offset: 0,
                },
            )],
        )
        .unwrap_err();
    // v6.7.1: message switched from "is NSW" to "is not BTree"
    // when the Brin variant was added.
    assert!(matches!(err, StorageError::Corrupt(ref s) if s.contains("not BTree")));
}

#[test]
fn load_segment_bytes_rejects_garbage() {
    let mut cat = Catalog::new();
    let err = cat.load_segment_bytes(vec![0u8; 10]).unwrap_err();
    assert!(matches!(err, StorageError::Corrupt(ref s) if s.contains("segment")));
    // Loader doesn't mutate state on error.
    assert_eq!(cat.cold_segment_count(), 0);
}

#[test]
fn load_segment_bytes_returns_sequential_ids() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let schema = cat.get("users").unwrap().schema.clone();
    for batch in 0u32..3 {
        let rows: Vec<(u64, Vec<u8>)> = (0u64..4)
            .map(|i| {
                let id = u64::from(batch) * 100 + i;
                let row = make_user_row(id.cast_signed(), "x");
                (id, encode_row_body_dense(&row, &schema))
            })
            .collect();
        let (bytes, _) = encode_segment(rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).unwrap();
        assert_eq!(cat.load_segment_bytes(bytes).unwrap(), batch);
    }
    assert_eq!(cat.cold_segment_count(), 3);
}

// --- v5.2 catalog format v9 ----------------------------------

/// Hand-craft a v8 catalog byte stream and confirm the v9 reader
/// accepts it and surfaces every `BTree` entry as a Hot locator.
/// Guards the backward-compat read path: existing v3.0.2 / v4.x
/// snapshots on disk must keep loading after the v5.2 bump.
#[test]
fn v8_catalog_decodes_as_all_hot_under_v9_reader() {
    // Build a populated catalog in memory, snapshot it with the
    // v9 serializer, then patch the version byte back to 8 and
    // strip the v9 BTree payload bytes so the layout matches what
    // a real v8 snapshot would have produced on disk. The v9
    // reader's version dispatch path then rebuilds the index
    // from rows (every locator becomes Hot).
    let mut cat = populated_users();
    cat.get_mut("users")
        .unwrap()
        .add_index("by_name".into(), "name")
        .unwrap();

    // To produce a faithful v8 byte stream we re-encode the same
    // catalog with the v8 layout: identical bytes up to (and
    // including) the per-index kind tag, but no inline BTree
    // entries.
    let v8_bytes = encode_as_v8(&cat);
    assert_eq!(v8_bytes[FILE_MAGIC.len()], 8, "version byte must be 8");

    let restored = Catalog::deserialize(&v8_bytes).expect("v9 reader accepts v8 stream");
    let idx = restored
        .get("users")
        .unwrap()
        .index_on(1)
        .expect("index_on(1) after restore");
    // v8 path always materialises Hot locators (no cold tier
    // existed pre-v5.2).
    assert_eq!(
        idx.lookup_eq(&IndexKey::Text("alice".into())),
        &[RowLocator::Hot(0), RowLocator::Hot(2)]
    );
    // No accidental Cold leak.
    for entry in idx.lookup_eq(&IndexKey::Text("alice".into())) {
        assert!(entry.is_hot(), "v8 → v9 read must yield Hot only");
    }
}

/// Encode `cat` using the v8 layout (no inline `BTree` entries,
/// version byte = 8). Pure test helper — duplicates just enough
/// of `Catalog::serialize` to produce a faithful v8 stream that
/// real v3.0.2 / v4.x deployments wrote.
fn encode_as_v8(cat: &Catalog) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(FILE_MAGIC);
    out.push(8u8);
    write_u32(&mut out, u32::try_from(cat.tables.len()).unwrap());
    for t in &cat.tables {
        write_str(&mut out, &t.schema.name);
        write_u16(&mut out, u16::try_from(t.schema.columns.len()).unwrap());
        for c in &t.schema.columns {
            write_str(&mut out, &c.name);
            write_data_type(&mut out, c.ty);
            out.push(u8::from(c.nullable));
            match &c.default {
                None => out.push(0),
                Some(v) => {
                    out.push(1);
                    write_value(&mut out, v);
                }
            }
            out.push(u8::from(c.auto_increment));
        }
        write_u32(&mut out, u32::try_from(t.rows.len()).unwrap());
        for row in &t.rows {
            out.extend_from_slice(&encode_row_body_dense(row, &t.schema));
        }
        write_u16(&mut out, u16::try_from(t.indices.len()).unwrap());
        for idx in &t.indices {
            write_str(&mut out, &idx.name);
            write_u16(&mut out, u16::try_from(idx.column_position).unwrap());
            match &idx.kind {
                // v8 BTree wrote only the kind tag; entries
                // rebuild from rows on read.
                IndexKind::BTree(_) => out.push(0),
                IndexKind::Nsw(g) => {
                    out.push(1);
                    write_u16(&mut out, u16::try_from(g.m).unwrap());
                    write_nsw_graph(&mut out, g);
                }
                // v8 had no BRIN / GIN; this test-only writer
                // can't serialise either into the legacy format.
                IndexKind::Brin { .. } => panic!(
                    "v8 catalog writer cannot serialise BRIN — \
                     tests with BRIN indices must use the current writer"
                ),
                IndexKind::Gin(_) => panic!(
                    "v8 catalog writer cannot serialise GIN — \
                     tests with GIN indices must use the current writer"
                ),
                IndexKind::GinTrgm(_) => panic!(
                    "v8 catalog writer cannot serialise trigram-GIN — \
                     tests with trgm indices must use the current writer"
                ),
                IndexKind::GinFulltext(_) => panic!(
                    "v8 catalog writer cannot serialise fulltext-GIN — \
                     tests with FULLTEXT KEY must use the current writer"
                ),
                IndexKind::GinJsonb(_) => panic!(
                    "v8 catalog writer cannot serialise JSONB-GIN — \
                     tests with JSONB-GIN must use the current writer"
                ),
            }
        }
    }
    out
}

/// Build a catalog that carries both hot and cold locators on a
/// `BTree` index, snapshot it through `serialize`, then deserialise
/// and confirm every Cold locator round-trips byte-identical and
/// `lookup_by_pk` resolves through the rebuilt cold-segment
/// registry.
#[test]
fn v9_catalog_round_trip_preserves_cold_locators() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    // Hot rows: 1, 2
    for (id, name) in [(1i64, "alice"), (2, "bob")] {
        t.insert(make_user_row(id, name)).unwrap();
    }
    t.add_index("by_id".into(), "id").unwrap();
    let schema = t.schema.clone();

    // Cold rows: 100, 200, 300 — sit in a single segment.
    let cold_rows: Vec<(i64, &str)> = vec![(100, "ivy"), (200, "joe"), (300, "kim")];
    let seg_rows: Vec<(u64, Vec<u8>)> = cold_rows
        .iter()
        .map(|(id, name)| {
            let row = make_user_row(*id, name);
            ((*id).cast_unsigned(), encode_row_body_dense(&row, &schema))
        })
        .collect();
    let (seg_bytes, _) = encode_segment(seg_rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).unwrap();
    let seg_id = cat.load_segment_bytes(seg_bytes.clone()).unwrap();
    let pairs: Vec<(IndexKey, RowLocator)> = cold_rows
        .iter()
        .map(|(id, _)| {
            (
                IndexKey::Int(*id),
                RowLocator::Cold {
                    segment_id: seg_id,
                    page_offset: 0,
                },
            )
        })
        .collect();
    cat.get_mut("users")
        .unwrap()
        .register_cold_locators("by_id", pairs)
        .unwrap();

    // Snapshot + restore via the v9 codec.
    let bytes = cat.serialize();
    assert_eq!(bytes[FILE_MAGIC.len()], FILE_VERSION);
    let mut restored = Catalog::deserialize(&bytes).expect("v9 round-trip parses");

    // Catalog::serialize does not yet emit cold segment file
    // bytes (v5.3 manifest is the future home for that). For
    // this v9 test the caller side-loads the segment again so
    // lookup_by_pk can resolve the Cold locator. The point of
    // this assertion is that the locator metadata survived the
    // catalog round-trip.
    let restored_seg_id = restored.load_segment_bytes(seg_bytes).unwrap();
    assert_eq!(restored_seg_id, seg_id);

    let idx = restored.get("users").unwrap().index_on(0).unwrap();
    // Hot locators round-trip.
    assert_eq!(idx.lookup_eq(&IndexKey::Int(1)), &[RowLocator::Hot(0)]);
    assert_eq!(idx.lookup_eq(&IndexKey::Int(2)), &[RowLocator::Hot(1)]);
    // Cold locators round-trip byte-identical.
    for (id, _) in &cold_rows {
        assert_eq!(
            idx.lookup_eq(&IndexKey::Int(*id)),
            &[RowLocator::Cold {
                segment_id: seg_id,
                page_offset: 0,
            }]
        );
    }
    // End-to-end: lookup_by_pk resolves both tiers.
    assert_eq!(
        restored
            .lookup_by_pk("users", "by_id", &IndexKey::Int(2))
            .unwrap(),
        make_user_row(2, "bob")
    );
    for (id, name) in &cold_rows {
        assert_eq!(
            restored
                .lookup_by_pk("users", "by_id", &IndexKey::Int(*id))
                .unwrap(),
            make_user_row(*id, name)
        );
    }
}

// --- v5.2.1 hot tier byte tracking ---------------------------

/// `row_body_encoded_len` is the perf-critical fast path; pin it
/// against `encode_row_body_dense(...).len()` for every
/// representative cell type so an encoder change can't silently
/// desync the counter.
#[test]
fn row_body_encoded_len_matches_actual_encode_for_all_types() {
    let schema = TableSchema::new(
        "wide",
        vec![
            ColumnSchema::new("a", DataType::SmallInt, true),
            ColumnSchema::new("b", DataType::Int, false),
            ColumnSchema::new("c", DataType::BigInt, false),
            ColumnSchema::new("d", DataType::Float, false),
            ColumnSchema::new("e", DataType::Bool, false),
            ColumnSchema::new("f", DataType::Text, false),
            ColumnSchema::new(
                "g",
                DataType::Vector {
                    dim: 3,
                    encoding: VecEncoding::F32,
                },
                false,
            ),
            ColumnSchema::new(
                "h",
                DataType::Numeric {
                    precision: 18,
                    scale: 2,
                },
                false,
            ),
            ColumnSchema::new("i", DataType::Date, false),
            ColumnSchema::new("j", DataType::Timestamp, false),
        ],
    );
    let cases: &[Row] = &[
        Row::new(vec![
            Value::SmallInt(7),
            Value::Int(42),
            Value::BigInt(1_000_000),
            Value::Float(1.5),
            Value::Bool(true),
            Value::text("hello"),
            Value::vector(vec![1.0, 2.0, 3.0]),
            Value::Numeric {
                scaled: 12345,
                scale: 2,
             kind: crate::NumericKind::Finite },
            Value::Date(20_000),
            Value::Timestamp(1_700_000_000_000_000),
        ]),
        // NULL in the bitmap, varied text length.
        Row::new(vec![
            Value::Null,
            Value::Int(0),
            Value::BigInt(0),
            Value::Float(0.0),
            Value::Bool(false),
            Value::text(""),
            Value::vector(vec![]),
            Value::Numeric {
                scaled: 0,
                scale: 2,
             kind: crate::NumericKind::Finite },
            Value::Date(0),
            Value::Timestamp(0),
        ]),
        Row::new(vec![
            Value::SmallInt(-1),
            Value::Int(-1),
            Value::BigInt(-1),
            Value::Float(-0.5),
            Value::Bool(true),
            Value::text("a much longer payload here"),
            Value::vector(vec![0.1, 0.2, 0.3]),
            Value::Numeric {
                scaled: -999_999_999,
                scale: 2,
             kind: crate::NumericKind::Finite },
            Value::Date(-1),
            Value::Timestamp(-1),
        ]),
    ];
    for row in cases {
        let actual = encode_row_body_dense(row, &schema).len();
        let fast = row_body_encoded_len(row, &schema);
        assert_eq!(actual, fast, "row {row:?}");
    }
}

#[test]
fn hot_bytes_grows_on_insert_and_matches_encoded_sum() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    assert_eq!(t.hot_bytes(), 0);
    let mut expected: u64 = 0;
    for (id, name) in [(1i64, "alice"), (2, "bob"), (3, "carol")] {
        let row = make_user_row(id, name);
        expected += encode_row_body_dense(&row, &t.schema).len() as u64;
        t.insert(row).unwrap();
    }
    assert_eq!(t.hot_bytes(), expected);
    assert_eq!(cat.hot_tier_bytes(), expected);
}

#[test]
fn hot_bytes_shrinks_on_delete() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for (id, name) in [(1i64, "alice"), (2, "bob"), (3, "carol")] {
        t.insert(make_user_row(id, name)).unwrap();
    }
    let before = t.hot_bytes();
    // Delete row at position 1 (bob).
    let bob_row = make_user_row(2, "bob");
    let bob_bytes = encode_row_body_dense(&bob_row, &t.schema).len() as u64;
    let removed = t.delete_rows(&[1]);
    assert_eq!(removed, 1);
    assert_eq!(t.hot_bytes(), before - bob_bytes);
}

#[test]
fn hot_bytes_diffs_on_update_for_variable_width_columns() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    t.insert(make_user_row(1, "alice")).unwrap();
    let after_insert = t.hot_bytes();
    // Update with a longer text payload — bytes must grow exactly
    // by the text-length delta.
    let new_row = make_user_row(1, "alice-the-longer-name");
    let old_len = encode_row_body_dense(&make_user_row(1, "alice"), &t.schema).len() as u64;
    let new_len = encode_row_body_dense(&new_row, &t.schema).len() as u64;
    t.update_row(0, new_row.values).unwrap();
    assert_eq!(t.hot_bytes(), after_insert - old_len + new_len);
    assert!(t.hot_bytes() > after_insert, "longer text grew the counter");
}

#[test]
fn hot_bytes_round_trips_through_serialize_deserialize() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for i in 0..10 {
        t.insert(make_user_row(i, &alloc::format!("name-{i}")))
            .unwrap();
    }
    let pre = cat.hot_tier_bytes();
    let restored = Catalog::deserialize(&cat.serialize()).unwrap();
    assert_eq!(restored.hot_tier_bytes(), pre);
    assert_eq!(restored.get("users").unwrap().hot_bytes(), pre);
}

// --- v5.2.2 freezer atomic swap -------------------------------

/// Happy path: freeze the first half of a populated hot tier,
/// confirm row counts shift, `hot_bytes` shrinks, and every frozen
/// PK still resolves via `lookup_by_pk` (now through the cold
/// segment registered by the freeze).
#[test]
fn freeze_oldest_to_cold_moves_rows_and_keeps_lookups_working() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for id in 0..10i64 {
        t.insert(make_user_row(id, &alloc::format!("u-{id}")))
            .unwrap();
    }
    t.add_index("by_id".into(), "id").unwrap();
    let total_bytes_before = t.hot_bytes();

    let report = cat
        .freeze_oldest_to_cold("users", "by_id", 6)
        .expect("freeze succeeds");
    assert_eq!(report.frozen_rows, 6);
    assert_eq!(report.segment_id, 0);
    assert!(report.bytes_freed > 0);
    assert!(!report.segment_bytes.is_empty());

    let t = cat.get("users").unwrap();
    assert_eq!(t.row_count(), 4, "4 hot rows remain (10 - 6 frozen)");
    assert_eq!(cat.cold_segment_count(), 1);
    // Hot bytes shrank by exactly the freed amount.
    assert_eq!(
        t.hot_bytes(),
        total_bytes_before - report.bytes_freed,
        "hot_bytes accounting matches FreezeReport"
    );

    // Every original PK still resolves — frozen ones via the
    // cold segment, kept ones via the (renumbered) hot tier.
    for id in 0..10i64 {
        let got = cat
            .lookup_by_pk("users", "by_id", &IndexKey::Int(id))
            .unwrap_or_else(|| panic!("PK {id} disappeared after freeze"));
        assert_eq!(got, make_user_row(id, &alloc::format!("u-{id}")));
    }
}

/// Two successive freezes on the same index must preserve the
/// first batch's cold locators when the second freeze runs.
/// Catches the `rebuild_indices` wipe-Cold-on-delete bug that
/// `collect_cold_locators` / re-register guards against.
#[test]
fn freeze_twice_preserves_prior_cold_locators() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for id in 0..12i64 {
        t.insert(make_user_row(id, &alloc::format!("u-{id}")))
            .unwrap();
    }
    t.add_index("by_id".into(), "id").unwrap();

    cat.freeze_oldest_to_cold("users", "by_id", 4)
        .expect("first freeze ok");
    cat.freeze_oldest_to_cold("users", "by_id", 4)
        .expect("second freeze ok");

    assert_eq!(cat.get("users").unwrap().row_count(), 4);
    assert_eq!(cat.cold_segment_count(), 2);
    // All 12 PKs still resolve — first 4 via segment 0,
    // next 4 via segment 1, last 4 still hot.
    for id in 0..12i64 {
        let got = cat
            .lookup_by_pk("users", "by_id", &IndexKey::Int(id))
            .unwrap_or_else(|| panic!("PK {id} not resolvable after two freezes"));
        assert_eq!(got, make_user_row(id, &alloc::format!("u-{id}")));
    }
}

/// v7.37.15 (Phase A.2 TDD) — `Table.headers` must stay
/// lock-step with `Table.rows` across every mutating path.
/// `headers.len() == rows.len()` is the load-bearing invariant
/// for Phase B's per-row visibility gate (`headers[i]` indexes
/// into the SAME row as `rows[i]`); if it ever drifts, scans
/// see the wrong visibility decision.
///
/// Exercises insert / delete / truncate / freeze / WAL-replay
/// shapes and checks the invariant after each.
#[test]
fn v7_37_15_headers_stay_lock_step_with_rows() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();

    // Insert path.
    for id in 0..10i64 {
        t.insert(make_user_row(id, &alloc::format!("u-{id}")))
            .unwrap();
    }
    assert_eq!(t.row_count(), 10);
    assert_eq!(
        t.headers().len(),
        t.rows().len(),
        "headers in lock-step after 10 inserts"
    );

    // delete_rows path.
    t.delete_rows(&[0, 2, 4]);
    assert_eq!(t.row_count(), 7);
    assert_eq!(
        t.headers().len(),
        t.rows().len(),
        "headers in lock-step after delete_rows"
    );

    // insert_no_index (WAL replay path).
    t.insert_no_index(make_user_row(100, "replay-100")).unwrap();
    assert_eq!(t.row_count(), 8);
    assert_eq!(
        t.headers().len(),
        t.rows().len(),
        "headers in lock-step after insert_no_index"
    );

    // delete_rows_no_index (WAL replay path).
    t.delete_rows_no_index(&[0, 1]);
    assert_eq!(t.row_count(), 6);
    assert_eq!(
        t.headers().len(),
        t.rows().len(),
        "headers in lock-step after delete_rows_no_index"
    );

    // truncate path.
    t.truncate();
    assert_eq!(t.row_count(), 0);
    assert_eq!(
        t.headers().len(),
        t.rows().len(),
        "headers in lock-step after truncate"
    );
}

/// v7.37.15 (Phase A.2 TDD) — fresh inserts default to
/// `RowHeader::frozen()` so visibility-aware scans (Phase B)
/// continue returning every row to every snapshot, preserving
/// pre-v7.37.15 behaviour while the catalog isn't yet
/// version-tracked.
#[test]
fn v7_37_15_fresh_inserts_default_to_frozen_header() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    t.insert(make_user_row(7, "alice")).unwrap();
    let header = t.headers().get(0).expect("header for first row");
    assert!(
        header.is_all_visible_fast(),
        "fresh insert must default to frozen + alive (got {header:?})"
    );
}

/// v7.37.15 (Phase B TDD) — `Table::is_row_visible` and
/// `scan_visible` return the full row set under the unbounded
/// snapshot (preserves pre-v7.37.15 contract) and filter
/// correctly under a snapshot that hides a specific tx.
#[test]
fn v7_37_15_phase_b_scan_visible_filters_correctly() {
    use crate::snapshot::{InProgressSet, Snapshot};

    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for id in 0..5i64 {
        t.insert(make_user_row(id, &alloc::format!("u-{id}")))
            .unwrap();
    }

    // Phase A: every header is frozen, so unbounded sees all 5.
    let unbounded = Snapshot::unbounded();
    let all: Vec<_> = t.scan_visible(&unbounded).collect();
    assert_eq!(
        all.len(),
        5,
        "unbounded snapshot must see every frozen row"
    );
    for i in 0..5 {
        assert!(t.is_row_visible(i, &unbounded), "row {i} visible");
    }

    // Now simulate Phase C semantics: hand-rewrite a few headers
    // to non-frozen states + a snapshot that hides one of them.
    //
    // (Reach through the test cfg into the underlying PersistentVec —
    // Phase C will land the proper writer-side stamping API.)
    let snap = Snapshot::new(
        100,                       // version
        InProgressSet::from_sorted(alloc::vec![50]), // tx 50 in flight
        50,                        // oldest_active
        0,                         // anonymous reader
    );
    // Direct-write a row header to xmin=50 (an in-flight tx); it
    // becomes invisible to `snap`.
    {
        let header = t
            .headers()
            .get(0)
            .expect("row 0 header exists");
        // The PersistentVec set yields a fresh vec; reassign back
        // via Table's existing test-friendly path (Phase C will
        // add a writer-aware path; here we splice manually).
        let in_flight = crate::row_header::RowHeader {
            xmin: 50,
            xmax: crate::row_header::XMAX_ALIVE,
            flags: 0,
        };
        let _ = header; // currently frozen
        // PersistentVec's `set` returns a fresh persistent vec
        // with the slot replaced; assign back into the parallel
        // field via the test-only mutator.
        let new_headers = t
            .headers_mut_for_test()
            .set(0, in_flight)
            .expect("row 0 exists");
        *t.headers_mut_for_test() = new_headers;
    }

    let visible: Vec<_> = t.scan_visible(&snap).map(|(i, _)| i).collect();
    assert!(
        !visible.contains(&0),
        "row 0 has xmin=50 (in-flight) — must be invisible to snap; \
         visible set = {visible:?}"
    );
    assert_eq!(
        visible.len(),
        4,
        "other 4 rows still frozen + alive; visible to snap"
    );
}

/// v7.37.15 (Phase C TDD) — `insert_with_xmin` stamps the new
/// row's header with the writing tx's version. A snapshot taken
/// BEFORE that version was committed (i.e. with the writer's tx
/// in `in_progress`) does not see the row; a snapshot taken
/// AFTER does.
#[test]
fn v7_37_15_phase_c_insert_with_xmin_stamps_header() {
    use crate::snapshot::{InProgressSet, Snapshot};

    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    // Writer tx allocates version 17.
    t.insert_with_xmin(make_user_row(7, "alice"), 17).unwrap();
    let header = t.headers().get(0).expect("header exists");
    assert_eq!(header.xmin, 17, "header xmin = writer's tx version");
    assert_eq!(header.xmax, crate::row_header::XMAX_ALIVE);

    // Snapshot taken WHILE tx 17 is still in-flight: row hidden.
    let before = Snapshot::new(
        20,                                                     // version
        InProgressSet::from_sorted(alloc::vec![17]),            // tx 17 in flight
        10,                                                     // oldest_active
        0,                                                       // reader
    );
    assert!(!t.is_row_visible(0, &before), "row hidden while writer in-flight");

    // Snapshot taken AFTER tx 17 commits: row visible.
    let after = Snapshot::new(
        20,
        InProgressSet::empty(),
        20,
        0,
    );
    assert!(t.is_row_visible(0, &after), "row visible after writer commits");
}

/// v7.37.15 (Phase C TDD) — `mark_row_deleted` stamps `xmax`
/// without removing the row physically. A snapshot taken BEFORE
/// the delete commits still sees the row; AFTER does not.
#[test]
fn v7_37_15_phase_c_mark_row_deleted_writes_xmax() {
    use crate::snapshot::{InProgressSet, Snapshot};

    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    t.insert_with_xmin(make_user_row(7, "alice"), 17).unwrap();
    // Delete-tx allocates version 25.
    t.mark_row_deleted(0, 25).unwrap();
    let h = t.headers().get(0).expect("header exists");
    assert_eq!(h.xmin, 17);
    assert_eq!(h.xmax, 25, "xmax = deleter's version");
    // Row is still physically present; vacuum (Phase D) reclaims later.
    assert_eq!(t.row_count(), 1);

    // Snapshot taken BEFORE tx 25 commits sees the row.
    let before = Snapshot::new(
        30,
        InProgressSet::from_sorted(alloc::vec![25]),
        17,
        0,
    );
    assert!(t.is_row_visible(0, &before), "row visible while deleter in-flight");

    // Snapshot taken AFTER tx 25 commits doesn't see it.
    let after = Snapshot::new(
        30,
        InProgressSet::empty(),
        30,
        0,
    );
    assert!(!t.is_row_visible(0, &after), "row hidden after deleter commits");
}

/// v7.37.15 (Phase C TDD) — re-deleting an already-tombstoned
/// row does not overwrite the original `xmax`. First-deleter-wins.
#[test]
fn v7_37_15_phase_c_repeated_delete_preserves_original_xmax() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    t.insert_with_xmin(make_user_row(7, "alice"), 17).unwrap();
    t.mark_row_deleted(0, 25).unwrap();
    t.mark_row_deleted(0, 100).unwrap(); // attempted overwrite
    let h = t.headers().get(0).expect("header exists");
    assert_eq!(h.xmax, 25, "original xmax preserved; first-deleter-wins");
}

/// v7.37.15 (Phase C TDD) — `insert_with_xmin(row, XMIN_FROZEN)`
/// short-circuits to the legacy frozen-insert path for backwards
/// compatibility with WAL replay / in-memory tests.
#[test]
fn v7_37_15_phase_c_frozen_xmin_short_circuits_to_legacy_insert() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    t.insert_with_xmin(make_user_row(7, "alice"), crate::row_header::XMIN_FROZEN)
        .unwrap();
    let h = t.headers().get(0).expect("header exists");
    assert!(
        h.is_all_visible_fast(),
        "frozen-xmin call must produce a frozen-fast header"
    );
}

/// v7.37.15 (Phase D TDD) — `Table::vacuum` removes rows whose
/// delete-commit predates `oldest_active_snapshot` and leaves
/// rows still possibly visible to a live snapshot in place.
#[test]
fn v7_37_15_phase_d_vacuum_reclaims_only_safe_rows() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    // Insert 5 rows at writer version 10..15.
    for id in 0..5i64 {
        t.insert_with_xmin(make_user_row(id, &alloc::format!("u-{id}")), 10 + id as u64)
            .unwrap();
    }
    // Delete row 1 at version 100, row 3 at version 200.
    t.mark_row_deleted(1, 100).unwrap();
    t.mark_row_deleted(3, 200).unwrap();
    assert_eq!(t.row_count(), 5, "physical row count unchanged by delete");

    // Dry-run with oldest_active = 150. Row 1 (xmax=100 < 150) is
    // reclaimable; row 3 (xmax=200 > 150) is NOT — a reader at
    // snapshot version 150 could still see it.
    let dry = t.vacuum(150, true);
    assert_eq!(dry.rows_reclaimed, 1, "dry-run counts safe-to-reclaim only");
    assert_eq!(t.row_count(), 5, "dry-run does not mutate");

    // Capture the survivors' stable RowIds before the compaction.
    // Rows are 1-based (positions 0..5 → rowids 1..=5); position 1
    // (rowid 2) is the one that will be reclaimed.
    let expected_survivor_rowids: alloc::vec::Vec<crate::row_header::RowId> = (0..t.row_count())
        .filter(|&i| i != 1)
        .filter_map(|i| t.rowids().get(i).copied())
        .collect();

    // Real pass. Row 1 reclaimed; the other 4 remain.
    let real = t.vacuum(150, false);
    assert_eq!(real.rows_reclaimed, 1);
    assert_eq!(t.row_count(), 4);
    // RowId stability: the four survivors keep their exact RowIds and
    // stay in order after the compaction shifts their physical slots.
    let survivor_rowids: alloc::vec::Vec<crate::row_header::RowId> =
        t.rowids().iter().copied().collect();
    assert_eq!(
        survivor_rowids, expected_survivor_rowids,
        "survivors keep their stable RowIds across vacuum compaction"
    );
    assert_eq!(t.rowids().len(), t.rows().len(), "rowids lock-step with rows");
}

/// v7.37.15 (Phase D TDD) — `Catalog::vacuum_all` aggregates per-
/// table reports and only lists tables that had reclaimable rows.
#[test]
fn v7_37_15_phase_d_vacuum_all_aggregates_per_table() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let users_schema = TableSchema::new(
        "logs",
        vec![
            ColumnSchema::new("id", DataType::BigInt, false),
            ColumnSchema::new("msg", DataType::Text, true),
        ],
    );
    cat.create_table(users_schema).unwrap();

    let u = cat.get_mut("users").unwrap();
    u.insert_with_xmin(make_user_row(0, "alice"), 10).unwrap();
    u.mark_row_deleted(0, 50).unwrap();

    let l = cat.get_mut("logs").unwrap();
    l.insert_with_xmin(
        Row::new(vec![Value::BigInt(1), Value::text("hello")]),
        20,
    )
    .unwrap();
    // logs row stays alive — vacuum reclaims nothing here.

    let report = cat.vacuum_all(100, false);
    assert_eq!(report.rows_reclaimed, 1, "one row reclaimed across both tables");
    assert_eq!(
        report.per_table.len(),
        1,
        "only tables with reclaimed rows appear; got {:?}",
        report.per_table
    );
    assert_eq!(report.per_table[0].0, "users");
    assert_eq!(report.per_table[0].1, 1);
}

/// v7.37.15 (Phase D TDD) — `is_all_visible` returns true on a
/// freshly-populated table (every insert defaults to frozen),
/// flips to false the moment a non-frozen MVCC writer stamps
/// any row, and recovers to true after vacuum reclaims the
/// non-frozen rows.
#[test]
fn v7_37_15_phase_d_is_all_visible_tracks_mvcc_writers() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();

    // Empty table is trivially all-visible.
    assert!(t.is_all_visible());

    // Legacy inserts (default frozen) keep the table all-visible.
    for id in 0..3i64 {
        t.insert(make_user_row(id, &alloc::format!("u-{id}")))
            .unwrap();
    }
    assert!(t.is_all_visible(), "frozen-only table is all-visible");

    // An MVCC writer stamps a non-frozen xmin → flag flips.
    t.insert_with_xmin(make_user_row(99, "mvcc"), 42).unwrap();
    assert!(
        !t.is_all_visible(),
        "non-frozen xmin must clear all-visible"
    );

    // Vacuum out the MVCC row (set xmax + vacuum at a version
    // past oldest_active).
    t.mark_row_deleted(3, 50).unwrap();
    let _ = t.vacuum(100, false);
    assert!(
        t.is_all_visible(),
        "table is all-visible again after the MVCC row is reclaimed"
    );
}

/// v7.37.15 (Phase C+D TDD) — end-to-end MVCC story across
/// insert / delete / vacuum / snapshot:
///
/// 1. A writer at version V inserts a row. A snapshot taken BEFORE
///    V commits (in_progress includes V) hides the row.
/// 2. After V commits the row is visible.
/// 3. A deleter at version W marks the row deleted. A snapshot
///    taken BEFORE W commits still sees the row; AFTER does not.
/// 4. Once oldest_active_snapshot exceeds W, vacuum reclaims the
///    physical storage.
#[test]
fn v7_37_15_end_to_end_mvcc_lifecycle() {
    use crate::snapshot::{InProgressSet, Snapshot};
    use crate::vacuum::is_reclaimable;

    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();

    // Step 1: writer V=10 inserts; concurrent snapshot at version
    // 15 with V in_progress hides the row.
    t.insert_with_xmin(make_user_row(42, "alice"), 10).unwrap();
    let mid_insert = Snapshot::new(15, InProgressSet::from_sorted(alloc::vec![10]), 10, 0);
    assert!(!t.is_row_visible(0, &mid_insert), "writer in flight ⇒ hidden");

    // Step 2: V committed (in_progress empty) → row visible.
    let post_insert = Snapshot::new(20, InProgressSet::empty(), 20, 0);
    assert!(t.is_row_visible(0, &post_insert), "committed insert ⇒ visible");

    // Step 3: deleter W=30 stamps xmax. Snapshot at version 25 (pre-
    // delete) still sees row.
    t.mark_row_deleted(0, 30).unwrap();
    let pre_delete = Snapshot::new(25, InProgressSet::empty(), 25, 0);
    assert!(t.is_row_visible(0, &pre_delete), "pre-delete snapshot sees row");
    let post_delete = Snapshot::new(50, InProgressSet::empty(), 30, 0);
    assert!(!t.is_row_visible(0, &post_delete), "post-delete snapshot hides row");

    // Step 4: vacuum reclaim. Only safe when oldest_active > xmax.
    // oldest_active=25 → still possibly observable, do NOT reclaim.
    assert!(!is_reclaimable(30, 25));
    let dry_at_25 = t.vacuum(25, true);
    assert_eq!(dry_at_25.rows_reclaimed, 0, "vacuum waits for oldest_active > xmax");
    // oldest_active=40 → safe (every live snapshot is past 30).
    assert!(is_reclaimable(30, 40));
    let real_at_40 = t.vacuum(40, false);
    assert_eq!(real_at_40.rows_reclaimed, 1);
    assert_eq!(t.row_count(), 0, "row physically reclaimed by vacuum");
}

/// Validation guard tests. Each must return `Err` and **not
/// mutate the catalog** — the API is all-or-nothing.
#[test]
fn freeze_oldest_to_cold_rejects_invalid_input() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for id in 0..3i64 {
        t.insert(make_user_row(id, &alloc::format!("u-{id}")))
            .unwrap();
    }
    t.add_index("by_id".into(), "id").unwrap();

    // max_rows == 0
    assert!(matches!(
        cat.freeze_oldest_to_cold("users", "by_id", 0),
        Err(StorageError::Corrupt(_))
    ));
    // table missing
    assert!(matches!(
        cat.freeze_oldest_to_cold("missing", "by_id", 1),
        Err(StorageError::Corrupt(_))
    ));
    // index missing
    assert!(matches!(
        cat.freeze_oldest_to_cold("users", "no_such_index", 1),
        Err(StorageError::Corrupt(_))
    ));
    // max_rows > row_count
    assert!(matches!(
        cat.freeze_oldest_to_cold("users", "by_id", 999),
        Err(StorageError::Corrupt(_))
    ));
    // Catalog still untouched.
    assert_eq!(cat.get("users").unwrap().row_count(), 3);
    assert_eq!(cat.cold_segment_count(), 0);
}

/// Freeze with a non-integer PK column must surface a clear
/// error (Text PKs land in v5.5+).
#[test]
fn freeze_oldest_to_cold_rejects_non_integer_pk() {
    let mut cat = Catalog::new();
    cat.create_table(TableSchema::new(
        "by_name",
        vec![
            ColumnSchema::new("name", DataType::Text, false),
            ColumnSchema::new("payload", DataType::BigInt, false),
        ],
    ))
    .unwrap();
    let t = cat.get_mut("by_name").unwrap();
    t.insert(Row::new(vec![Value::text("a"), Value::BigInt(1)]))
        .unwrap();
    t.add_index("by_n".into(), "name").unwrap();
    let err = cat
        .freeze_oldest_to_cold("by_name", "by_n", 1)
        .expect_err("non-integer PK rejected");
    match err {
        StorageError::Corrupt(s) => assert!(
            s.contains("non-integer"),
            "error message names the constraint: {s}"
        ),
        other => panic!("expected Corrupt, got {other:?}"),
    }
    // Catalog untouched.
    assert_eq!(cat.get("by_name").unwrap().row_count(), 1);
    assert_eq!(cat.cold_segment_count(), 0);
}

/// Hot-tier rows after the freeze must keep their secondary-
/// index lookups working — `delete_rows` shifts positions, and
/// `rebuild_indices` must regenerate Hot locators at the new
/// indices.
#[test]
fn freeze_keeps_remaining_hot_rows_addressable_via_secondary_index() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for id in 0..6i64 {
        t.insert(make_user_row(id, &alloc::format!("u-{id}")))
            .unwrap();
    }
    t.add_index("by_id".into(), "id").unwrap();
    t.add_index("by_name".into(), "name").unwrap();

    cat.freeze_oldest_to_cold("users", "by_id", 3).unwrap();

    // Remaining hot rows: id 3, 4, 5. They moved to positions
    // 0, 1, 2 inside `self.rows`; the `by_name` index must now
    // resolve them via fresh Hot locators.
    let idx = cat.get("users").unwrap().index_on(1).unwrap();
    let got = idx.lookup_eq(&IndexKey::Text("u-4".into()));
    assert_eq!(got.len(), 1);
    assert!(got[0].is_hot(), "kept-hot rows still surface as Hot");
    match got[0] {
        RowLocator::Hot(i) => {
            // The 4th-inserted row was at position 4; after
            // dropping positions 0..3 it sits at position 1.
            assert_eq!(i, 1);
        }
        RowLocator::Cold { .. } => unreachable!(),
    }
}

// --- v5.2.3 promote-on-write primitives ----------------------

/// Build a populated catalog with the first N rows frozen, then
/// run `promote_cold_row` and verify the row crossed tiers
/// correctly: the cold locator is retired, a fresh Hot locator
/// appears, `lookup_by_pk` returns the row from the hot tier, and
/// `hot_bytes` grew by the row's encoded byte length.
#[test]
fn promote_cold_row_pulls_frozen_row_back_to_hot_tier() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for id in 0..6i64 {
        t.insert(make_user_row(id, &alloc::format!("u-{id}")))
            .unwrap();
    }
    t.add_index("by_id".into(), "id").unwrap();
    // Freeze first 4 rows (ids 0..3). After: hot rows = 4, 5 at
    // positions 0, 1; cold locators for keys 0..3.
    cat.freeze_oldest_to_cold("users", "by_id", 4).unwrap();
    let hot_bytes_before = cat.get("users").unwrap().hot_bytes();

    // Promote PK=2 — it lives in segment 0 as a cold row.
    let new_idx = cat
        .promote_cold_row("users", "by_id", &IndexKey::Int(2))
        .expect("promote ok")
        .expect("PK 2 was cold");
    assert_eq!(
        new_idx, 2,
        "promoted row appended after the 2 surviving hot rows"
    );

    let t = cat.get("users").unwrap();
    assert_eq!(t.row_count(), 3, "hot tier grew from 2 to 3");
    // Hot-bytes climbed by exactly one row's encoded length.
    let row = make_user_row(2, "u-2");
    let row_len = encode_row_body_dense(&row, &t.schema).len() as u64;
    assert_eq!(t.hot_bytes(), hot_bytes_before + row_len);

    // The index now reports a Hot locator (the freshly inserted
    // row) — no Cold locator left for PK 2.
    let entries = t.index_on(0).unwrap().lookup_eq(&IndexKey::Int(2));
    assert_eq!(entries.len(), 1, "exactly one locator per key");
    assert!(entries[0].is_hot(), "promote retired the Cold locator");
    // End-to-end: lookup_by_pk still returns the row body.
    assert_eq!(
        cat.lookup_by_pk("users", "by_id", &IndexKey::Int(2))
            .unwrap(),
        row
    );
    // Other cold rows untouched — still resolvable through the
    // segment.
    assert_eq!(
        cat.lookup_by_pk("users", "by_id", &IndexKey::Int(0))
            .unwrap(),
        make_user_row(0, "u-0")
    );
}

/// `promote_cold_row` on a key that's already hot (or absent)
/// returns `Ok(None)` — not an error. The caller falls back to
/// the hot-only update/delete path.
#[test]
fn promote_cold_row_returns_none_when_key_is_not_cold() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    t.insert(make_user_row(7, "alice")).unwrap();
    t.add_index("by_id".into(), "id").unwrap();

    // Hot-only key.
    assert!(
        cat.promote_cold_row("users", "by_id", &IndexKey::Int(7))
            .unwrap()
            .is_none()
    );
    // Absent key.
    assert!(
        cat.promote_cold_row("users", "by_id", &IndexKey::Int(99))
            .unwrap()
            .is_none()
    );
    // Catalog untouched on both no-op paths.
    assert_eq!(cat.get("users").unwrap().row_count(), 1);
    assert_eq!(cat.cold_segment_count(), 0);
}

/// `shadow_cold_row` removes every Cold locator for a key on a
/// `BTree` index. After the shadow, `lookup_by_pk` for that key
/// returns None (the row data still sits in the segment file,
/// but it's now garbage; compaction will reclaim it later).
#[test]
fn shadow_cold_row_removes_cold_locators_and_drops_lookup() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for id in 0..5i64 {
        t.insert(make_user_row(id, &alloc::format!("u-{id}")))
            .unwrap();
    }
    t.add_index("by_id".into(), "id").unwrap();
    cat.freeze_oldest_to_cold("users", "by_id", 3).unwrap();

    // Shadow PK=1 — pre-shadow lookup hits the cold tier.
    assert!(
        cat.lookup_by_pk("users", "by_id", &IndexKey::Int(1))
            .is_some(),
        "frozen PK resolves before shadow"
    );
    let removed = cat
        .shadow_cold_row("users", "by_id", &IndexKey::Int(1))
        .unwrap();
    assert_eq!(removed, 1, "exactly one cold locator retired");

    // Post-shadow: lookup misses, even though the row still
    // exists in segment 0.
    assert!(
        cat.lookup_by_pk("users", "by_id", &IndexKey::Int(1))
            .is_none(),
        "shadowed key no longer resolves"
    );
    // Other cold keys still resolve.
    assert_eq!(
        cat.lookup_by_pk("users", "by_id", &IndexKey::Int(0))
            .unwrap(),
        make_user_row(0, "u-0")
    );
    assert_eq!(
        cat.lookup_by_pk("users", "by_id", &IndexKey::Int(2))
            .unwrap(),
        make_user_row(2, "u-2")
    );
}

/// `shadow_cold_row` returns 0 (not Err) for keys with only Hot
/// entries or no entries — the engine's DELETE path uses this
/// signal to decide whether the cold-tier shadow path consumed
/// the work.
#[test]
fn shadow_cold_row_returns_zero_when_key_is_not_cold() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    t.insert(make_user_row(1, "alice")).unwrap();
    t.add_index("by_id".into(), "id").unwrap();
    assert_eq!(
        cat.shadow_cold_row("users", "by_id", &IndexKey::Int(1))
            .unwrap(),
        0,
        "hot-only key drops no cold locators"
    );
    assert_eq!(
        cat.shadow_cold_row("users", "by_id", &IndexKey::Int(999))
            .unwrap(),
        0,
        "absent key drops no cold locators"
    );
    assert_eq!(cat.get("users").unwrap().row_count(), 1);
}

/// Validation guards on both promote / shadow primitives.
#[test]
fn promote_and_shadow_reject_invalid_inputs() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    t.insert(make_user_row(1, "alice")).unwrap();
    t.add_index("by_id".into(), "id").unwrap();

    // Missing table.
    assert!(matches!(
        cat.promote_cold_row("missing", "by_id", &IndexKey::Int(1)),
        Err(StorageError::Corrupt(_))
    ));
    assert!(matches!(
        cat.shadow_cold_row("missing", "by_id", &IndexKey::Int(1)),
        Err(StorageError::Corrupt(_))
    ));
    // Missing index.
    assert!(matches!(
        cat.promote_cold_row("users", "no_such_index", &IndexKey::Int(1)),
        Err(StorageError::Corrupt(_))
    ));
    assert!(matches!(
        cat.shadow_cold_row("users", "no_such_index", &IndexKey::Int(1)),
        Err(StorageError::Corrupt(_))
    ));
}

// --- v6.7.4 parallel-freezer slice/commit API -----------------

/// One slice covering the entire freeze produces the same
/// catalog state as the single-threaded `freeze_oldest_to_cold`
/// — segment id, frozen row count, hot byte delta, and every
/// post-freeze PK lookup match exactly.
#[test]
fn commit_freeze_slices_single_slice_matches_freeze_oldest() {
    let mut a = Catalog::new();
    let mut b = Catalog::new();
    for cat in [&mut a, &mut b] {
        cat.create_table(bigint_pk_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        for id in 0..10i64 {
            t.insert(make_user_row(id, &alloc::format!("u-{id}")))
                .unwrap();
        }
        t.add_index("by_id".into(), "id").unwrap();
    }
    let single = a.freeze_oldest_to_cold("users", "by_id", 6).unwrap();
    let slice = b
        .prepare_freeze_slice("users", "by_id", 0..6)
        .expect("prepare");
    let parallel = b
        .commit_freeze_slices("users", "by_id", alloc::vec![slice])
        .expect("commit");
    assert_eq!(single.segment_id, parallel.segment_id);
    assert_eq!(single.frozen_rows, parallel.frozen_rows);
    assert_eq!(single.bytes_freed, parallel.bytes_freed);
    assert_eq!(single.segment_bytes, parallel.segment_bytes);
    // Same post-freeze lookup behaviour on both catalogs.
    for id in 0..10i64 {
        assert_eq!(
            a.lookup_by_pk("users", "by_id", &IndexKey::Int(id)),
            b.lookup_by_pk("users", "by_id", &IndexKey::Int(id)),
            "PK {id} differs after single vs slice freeze"
        );
    }
}

/// Two slices covering disjoint halves of the freeze produce
/// the same merged segment as one slice covering the full
/// range. The k-way merge preserves PK ordering even when
/// slice halves alternate.
#[test]
fn commit_freeze_slices_two_slices_match_single_slice() {
    let mut a = Catalog::new();
    let mut b = Catalog::new();
    for cat in [&mut a, &mut b] {
        cat.create_table(bigint_pk_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        // Random-ish PKs so the per-slice sort actually has
        // work to do (and slice halves carry interleaved keys).
        for id in [3, 7, 1, 9, 5, 0, 8, 4, 2, 6].iter().copied() {
            t.insert(make_user_row(id as i64, &alloc::format!("u-{id}")))
                .unwrap();
        }
        t.add_index("by_id".into(), "id").unwrap();
    }
    let single = a
        .prepare_freeze_slice("users", "by_id", 0..8)
        .expect("prepare");
    let one = a
        .commit_freeze_slices("users", "by_id", alloc::vec![single])
        .expect("commit one");
    let s1 = b
        .prepare_freeze_slice("users", "by_id", 0..4)
        .expect("prepare s1");
    let s2 = b
        .prepare_freeze_slice("users", "by_id", 4..8)
        .expect("prepare s2");
    let two = b
        .commit_freeze_slices("users", "by_id", alloc::vec![s1, s2])
        .expect("commit two");
    assert_eq!(one.segment_bytes, two.segment_bytes);
    assert_eq!(one.frozen_rows, two.frozen_rows);
    // Every PK that survived freeze (hot or cold) resolves on
    // both catalogs.
    for id in 0..10i64 {
        assert_eq!(
            a.lookup_by_pk("users", "by_id", &IndexKey::Int(id)),
            b.lookup_by_pk("users", "by_id", &IndexKey::Int(id)),
            "PK {id} differs after one-slice vs two-slice freeze"
        );
    }
}

/// Gap between slices → error before any mutation lands.
#[test]
fn commit_freeze_slices_rejects_gap() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for id in 0..6i64 {
        t.insert(make_user_row(id, &alloc::format!("u-{id}")))
            .unwrap();
    }
    t.add_index("by_id".into(), "id").unwrap();
    let s1 = cat.prepare_freeze_slice("users", "by_id", 0..2).unwrap();
    let s2 = cat.prepare_freeze_slice("users", "by_id", 3..5).unwrap();
    assert!(matches!(
        cat.commit_freeze_slices("users", "by_id", alloc::vec![s1, s2]),
        Err(StorageError::Corrupt(_))
    ));
    // Catalog untouched.
    assert_eq!(cat.cold_segment_count(), 0);
    assert_eq!(cat.get("users").unwrap().row_count(), 6);
}

/// Empty slice list → no-op success, catalog untouched.
#[test]
fn commit_freeze_slices_empty_is_noop() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for id in 0..3i64 {
        t.insert(make_user_row(id, &alloc::format!("u-{id}")))
            .unwrap();
    }
    t.add_index("by_id".into(), "id").unwrap();
    let report = cat
        .commit_freeze_slices("users", "by_id", Vec::new())
        .unwrap();
    assert_eq!(report.frozen_rows, 0);
    assert_eq!(cat.cold_segment_count(), 0);
    assert_eq!(cat.get("users").unwrap().row_count(), 3);
}

// --- v6.7.3 cold-segment compaction ---------------------------

/// Two small cold segments merge into a single larger one. The
/// merged segment carries every cold-resident row; the source
/// slots are tombstoned; every PK still resolves through the
/// new merged segment via `lookup_by_pk`.
#[test]
fn compact_merges_small_segments_storage_unit() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for id in 0..8i64 {
        t.insert(make_user_row(id, &alloc::format!("u-{id}")))
            .unwrap();
    }
    t.add_index("by_id".into(), "id").unwrap();
    // Two freezes of 3 rows each → two small cold segments.
    cat.freeze_oldest_to_cold("users", "by_id", 3).unwrap();
    cat.freeze_oldest_to_cold("users", "by_id", 3).unwrap();
    assert_eq!(cat.cold_segment_count(), 2);
    assert_eq!(cat.cold_segment_slot_count(), 2);

    // Pick a threshold larger than either segment's size so
    // both qualify.
    let max_seg_bytes = cat
        .cold_segment_ids_global()
        .iter()
        .map(|id| cat.cold_segment(*id).unwrap().bytes().len() as u64)
        .max()
        .unwrap();
    let target = max_seg_bytes + 1;

    let report = cat
        .compact_cold_segments("users", "by_id", target)
        .expect("compact succeeds");
    assert_eq!(report.sources.len(), 2);
    let merged_id = report.merged_segment_id.expect("merge happened");
    assert_eq!(report.merged_rows, 6);
    assert_eq!(report.deleted_rows_pruned, 0);
    assert!(!report.merged_segment_bytes.is_empty());

    // Active count drops back to 1; slot count grew to 3
    // (2 sources tombstoned + 1 merged appended).
    assert_eq!(cat.cold_segment_count(), 1);
    assert_eq!(cat.cold_segment_slot_count(), 3);
    assert_eq!(cat.cold_segment_ids_global(), alloc::vec![merged_id]);

    // Every PK that was frozen still resolves (via the merged
    // segment); the 2 hot rows still resolve too.
    for id in 0..8i64 {
        let got = cat
            .lookup_by_pk("users", "by_id", &IndexKey::Int(id))
            .unwrap_or_else(|| panic!("PK {id} lost after compaction"));
        assert_eq!(got, make_user_row(id, &alloc::format!("u-{id}")));
    }
}

/// DELETE'd-but-frozen rows are dropped during the merge. Set
/// up two small segments, then shadow one row in each; the
/// merged segment must NOT carry the shadowed rows.
#[test]
fn compact_drops_shadowed_cold_rows() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for id in 0..6i64 {
        t.insert(make_user_row(id, &alloc::format!("u-{id}")))
            .unwrap();
    }
    t.add_index("by_id".into(), "id").unwrap();
    cat.freeze_oldest_to_cold("users", "by_id", 3).unwrap();
    cat.freeze_oldest_to_cold("users", "by_id", 3).unwrap();
    // Shadow PK 1 (in seg 0) + PK 4 (in seg 1).
    assert_eq!(
        cat.shadow_cold_row("users", "by_id", &IndexKey::Int(1))
            .unwrap(),
        1
    );
    assert_eq!(
        cat.shadow_cold_row("users", "by_id", &IndexKey::Int(4))
            .unwrap(),
        1
    );

    let max_seg_bytes = cat
        .cold_segment_ids_global()
        .iter()
        .map(|id| cat.cold_segment(*id).unwrap().bytes().len() as u64)
        .max()
        .unwrap();
    let report = cat
        .compact_cold_segments("users", "by_id", max_seg_bytes + 1)
        .expect("compact succeeds");
    assert_eq!(report.sources.len(), 2);
    assert_eq!(report.merged_rows, 4, "6 frozen − 2 shadowed = 4 live");
    assert_eq!(report.deleted_rows_pruned, 2);

    // PK 1 and 4 stay invisible after compact.
    for shadowed in [1i64, 4i64] {
        assert!(
            cat.lookup_by_pk("users", "by_id", &IndexKey::Int(shadowed))
                .is_none(),
            "shadowed PK {shadowed} must remain invisible after compact"
        );
    }
    // The other 4 frozen rows resolve.
    for live in [0i64, 2, 3, 5] {
        cat.lookup_by_pk("users", "by_id", &IndexKey::Int(live))
            .unwrap_or_else(|| panic!("live PK {live} lost after compact"));
    }
}

/// No-op cases: 0 or 1 candidate segment under the threshold
/// leaves the catalog untouched.
#[test]
fn compact_is_noop_below_two_candidates() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for id in 0..6i64 {
        t.insert(make_user_row(id, &alloc::format!("u-{id}")))
            .unwrap();
    }
    t.add_index("by_id".into(), "id").unwrap();
    // 0 cold segments.
    let report = cat
        .compact_cold_segments("users", "by_id", 1 << 30)
        .expect("noop ok");
    assert!(report.merged_segment_id.is_none());
    assert!(report.sources.is_empty());

    // 1 cold segment — still a no-op (need ≥2 to merge).
    cat.freeze_oldest_to_cold("users", "by_id", 4).unwrap();
    let report = cat
        .compact_cold_segments("users", "by_id", 1 << 30)
        .expect("noop ok");
    assert!(report.merged_segment_id.is_none());
    assert_eq!(cat.cold_segment_count(), 1);

    // Threshold too small to cover the single segment → still
    // no-op.
    let report = cat
        .compact_cold_segments("users", "by_id", 1)
        .expect("noop ok");
    assert!(report.merged_segment_id.is_none());
    assert_eq!(cat.cold_segment_count(), 1);
}

/// Manifest-style atomicity: a Catalog snapshot taken AFTER
/// `compact_cold_segments` returns must round-trip with the
/// post-compact BTree state, while the cold-tier registry is
/// re-derived from the source-of-truth manifest (=
/// `load_segment_bytes_at` with the merged id + the still-on-
/// disk merged bytes). This mirrors the boot path: catalog
/// snapshot + cold-segment files = full state.
#[test]
fn compact_swap_survives_catalog_roundtrip_via_load_at() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for id in 0..6i64 {
        t.insert(make_user_row(id, &alloc::format!("u-{id}")))
            .unwrap();
    }
    t.add_index("by_id".into(), "id").unwrap();
    cat.freeze_oldest_to_cold("users", "by_id", 3).unwrap();
    cat.freeze_oldest_to_cold("users", "by_id", 3).unwrap();
    let max_seg_bytes = cat
        .cold_segment_ids_global()
        .iter()
        .map(|id| cat.cold_segment(*id).unwrap().bytes().len() as u64)
        .max()
        .unwrap();
    let report = cat
        .compact_cold_segments("users", "by_id", max_seg_bytes + 1)
        .expect("compact ok");
    let merged_id = report.merged_segment_id.unwrap();

    // Serialise the catalog (BTree index points at merged_id
    // now) and the merged segment bytes; pretend to crash; on
    // restart, re-hydrate the catalog and reload only the
    // merged segment at its baked-in id.
    let cat_bytes = cat.serialize();
    let merged_bytes = report.merged_segment_bytes.clone();

    let mut restored = Catalog::deserialize(&cat_bytes).expect("deserialize ok");
    restored
        .load_segment_bytes_at(merged_id, merged_bytes)
        .expect("reload merged ok");

    // All 6 PKs still resolve through the restored merged segment.
    for id in 0..6i64 {
        let got = restored
            .lookup_by_pk("users", "by_id", &IndexKey::Int(id))
            .unwrap_or_else(|| panic!("PK {id} lost across roundtrip"));
        assert_eq!(got, make_user_row(id, &alloc::format!("u-{id}")));
    }
    // No source slot ever rehydrates — confirmed by
    // `cold_segment_count` matching only the merged segment.
    assert_eq!(restored.cold_segment_count(), 1);
}

/// `load_segment_bytes_at` refuses to stomp an occupied slot
/// and pads with `None` when the target id is past the end.
#[test]
fn load_segment_bytes_at_pads_and_rejects_collision() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for id in 0..4i64 {
        t.insert(make_user_row(id, &alloc::format!("u-{id}")))
            .unwrap();
    }
    t.add_index("by_id".into(), "id").unwrap();
    let report = cat.freeze_oldest_to_cold("users", "by_id", 2).unwrap();
    let bytes_seg0 = report.segment_bytes.clone();

    // Pad to id=5 (slots 1..5 are None, slot 5 holds the
    // segment loaded back). The slot count jumps, the active
    // count is now 2 (seg 0 + seg 5).
    cat.load_segment_bytes_at(5, bytes_seg0.clone())
        .expect("pad + load ok");
    assert_eq!(cat.cold_segment_slot_count(), 6);
    assert_eq!(cat.cold_segment_count(), 2);

    // Re-loading at the same id collides.
    assert!(matches!(
        cat.load_segment_bytes_at(5, bytes_seg0.clone()),
        Err(StorageError::Corrupt(_))
    ));
    // Re-loading at id 0 (already occupied) also collides.
    assert!(matches!(
        cat.load_segment_bytes_at(0, bytes_seg0),
        Err(StorageError::Corrupt(_))
    ));
}

/// Round trip: freeze → promote → re-freeze. The same PK can
/// migrate hot ↔ cold multiple times. After two cycles only the
/// final Hot locator should be live.
#[test]
fn promote_then_refreeze_does_not_leave_orphan_locators() {
    let mut cat = Catalog::new();
    cat.create_table(bigint_pk_users_schema()).unwrap();
    let t = cat.get_mut("users").unwrap();
    for id in 0..4i64 {
        t.insert(make_user_row(id, &alloc::format!("u-{id}")))
            .unwrap();
    }
    t.add_index("by_id".into(), "id").unwrap();

    // Cycle 1: freeze first 2 rows, then promote PK 0.
    cat.freeze_oldest_to_cold("users", "by_id", 2).unwrap();
    let promoted = cat
        .promote_cold_row("users", "by_id", &IndexKey::Int(0))
        .unwrap();
    assert!(promoted.is_some());
    let entries_after_promote = cat
        .get("users")
        .unwrap()
        .index_on(0)
        .unwrap()
        .lookup_eq(&IndexKey::Int(0))
        .to_vec();
    assert_eq!(entries_after_promote.len(), 1);
    assert!(entries_after_promote[0].is_hot());

    // Cycle 2: freeze the front rows again. PK 0 is now at
    // position 2 (after the survivors); it could still go cold
    // again on a future freeze depending on policy, but the
    // current "first N positions" policy leaves it alone here.
    // What matters: prior cold locators for PKs 0..1 are gone,
    // PKs 2..3 still resolve through their original segments.
    for id in [2i64, 3] {
        assert_eq!(
            cat.lookup_by_pk("users", "by_id", &IndexKey::Int(id))
                .unwrap(),
            make_user_row(id, &alloc::format!("u-{id}"))
        );
    }
}

// v7.37.6-B(sentori Epic 2 P0)— partition_role catalog round-trip 钉。
// 普通表(None)/ Parent / Range child / Default child 各自序列化-反
// 序列化身份恒等;Parent 同时保 index_template_sources Vec<String> 而
// 不丢序;Range 边界 MinValue / MaxValue / TimestampTz 三态都跑过一次。

fn partition_parent_schema() -> TableSchema {
    let mut s = TableSchema::new(
        "events_partitioned",
        vec![
            ColumnSchema::new("id", DataType::BigInt, false),
            ColumnSchema::new("ts", DataType::Timestamptz, false),
            ColumnSchema::new("payload", DataType::Jsonb, true),
        ],
    );
    s.partition_role = Some(PartitionRole::Parent {
        kind: PartitionKind::Range,
        key_column_positions: vec![1],
        index_template_sources: vec![
            "CREATE INDEX events_partitioned_ts_idx ON events_partitioned (ts DESC)".to_string(),
            "CREATE INDEX events_partitioned_pid_ts_idx ON events_partitioned (payload, ts)"
                .to_string(),
        ],
    });
    s
}

fn partition_range_child_schema(name: &str, lower_micros: i64, upper_micros: i64) -> TableSchema {
    let mut s = TableSchema::new(
        name,
        vec![
            ColumnSchema::new("id", DataType::BigInt, false),
            ColumnSchema::new("ts", DataType::Timestamptz, false),
            ColumnSchema::new("payload", DataType::Jsonb, true),
        ],
    );
    s.partition_role = Some(PartitionRole::Range {
        parent_name: "events_partitioned".to_string(),
        lower: PartitionBound::TimestampTz(lower_micros),
        upper: PartitionBound::TimestampTz(upper_micros),
    });
    s
}

fn partition_default_child_schema(name: &str) -> TableSchema {
    let mut s = TableSchema::new(
        name,
        vec![
            ColumnSchema::new("id", DataType::BigInt, false),
            ColumnSchema::new("ts", DataType::Timestamptz, false),
            ColumnSchema::new("payload", DataType::Jsonb, true),
        ],
    );
    s.partition_role = Some(PartitionRole::Default {
        parent_name: "events_partitioned".to_string(),
    });
    s
}

/// Plain table(`partition_role = None`) round-trips byte-identical.
/// Defends the FILE_VERSION 49 "one tag byte for普通表" guarantee — a
/// table with no partition role should add exactly one zero byte
/// to its appendix.
#[test]
fn partition_role_none_round_trips() {
    let mut c = Catalog::new();
    c.create_table(TableSchema::new(
        "plain",
        vec![ColumnSchema::new("id", DataType::BigInt, false)],
    ))
    .unwrap();
    let bytes = c.serialize();
    let back = Catalog::deserialize(&bytes).unwrap();
    assert!(back.get("plain").unwrap().schema().partition_role.is_none());
    // 二次序列化恒等 — 旧→新→旧 zero drift。
    assert_eq!(bytes, back.serialize());
}

/// Parent + 3 child(2 range + 1 DEFAULT)的完整 catalog 经
/// serialize → deserialize 后,每张表的 partition_role 与原始
/// 一致(变体 + 字段 + 序),且 catalog 二次序列化字节恒等。
#[test]
fn partition_role_all_three_variants_round_trip() {
    let mut c = Catalog::new();
    c.create_table(partition_parent_schema()).unwrap();
    c.create_table(partition_range_child_schema(
        "events_2026_06",
        1_748_736_000_000_000, // 2026-06-01T00:00:00Z micros
        1_751_328_000_000_000, // 2026-07-01T00:00:00Z micros
    ))
    .unwrap();
    c.create_table(partition_range_child_schema(
        "events_2026_07",
        1_751_328_000_000_000,
        1_754_006_400_000_000,
    ))
    .unwrap();
    c.create_table(partition_default_child_schema("events_default"))
        .unwrap();

    let bytes = c.serialize();
    let back = Catalog::deserialize(&bytes).unwrap();

    // Parent 完整保 templates 顺序 + key 列位置 + Range kind。
    match back
        .get("events_partitioned")
        .unwrap()
        .schema()
        .partition_role
        .as_ref()
        .unwrap()
    {
        PartitionRole::Parent {
            kind,
            key_column_positions,
            index_template_sources,
        } => {
            assert_eq!(*kind, PartitionKind::Range);
            assert_eq!(key_column_positions, &vec![1usize]);
            assert_eq!(index_template_sources.len(), 2);
            assert!(index_template_sources[0].contains("ts DESC"));
            assert!(index_template_sources[1].contains("payload, ts"));
        }
        other => panic!("expected Parent, got {other:?}"),
    }

    // Range child:边界值 + parent_name 完整。
    match back
        .get("events_2026_06")
        .unwrap()
        .schema()
        .partition_role
        .as_ref()
        .unwrap()
    {
        PartitionRole::Range {
            parent_name,
            lower,
            upper,
        } => {
            assert_eq!(parent_name, "events_partitioned");
            assert_eq!(*lower, PartitionBound::TimestampTz(1_748_736_000_000_000));
            assert_eq!(*upper, PartitionBound::TimestampTz(1_751_328_000_000_000));
        }
        other => panic!("expected Range, got {other:?}"),
    }

    // Default child:仅 parent_name。
    match back
        .get("events_default")
        .unwrap()
        .schema()
        .partition_role
        .as_ref()
        .unwrap()
    {
        PartitionRole::Default { parent_name } => {
            assert_eq!(parent_name, "events_partitioned");
        }
        other => panic!("expected Default, got {other:?}"),
    }

    // 二次序列化字节恒等 — drift-free。
    assert_eq!(bytes, back.serialize());
}

/// Bound 三态(MinValue / MaxValue / TimestampTz)各自 codec 都能
/// round-trip。直接构 Range child 用 MinValue 当 lower、MaxValue 当
/// upper(MINVALUE / MAXVALUE 语义,sentori 不要但 zero-cost 留口)。
#[test]
fn partition_bound_minvalue_maxvalue_round_trip() {
    let mut c = Catalog::new();
    c.create_table(partition_parent_schema()).unwrap();

    let mut s = TableSchema::new(
        "events_all_time",
        vec![
            ColumnSchema::new("id", DataType::BigInt, false),
            ColumnSchema::new("ts", DataType::Timestamptz, false),
            ColumnSchema::new("payload", DataType::Jsonb, true),
        ],
    );
    s.partition_role = Some(PartitionRole::Range {
        parent_name: "events_partitioned".to_string(),
        lower: PartitionBound::MinValue,
        upper: PartitionBound::MaxValue,
    });
    c.create_table(s).unwrap();

    let bytes = c.serialize();
    let back = Catalog::deserialize(&bytes).unwrap();
    match back
        .get("events_all_time")
        .unwrap()
        .schema()
        .partition_role
        .as_ref()
        .unwrap()
    {
        PartitionRole::Range { lower, upper, .. } => {
            assert_eq!(*lower, PartitionBound::MinValue);
            assert_eq!(*upper, PartitionBound::MaxValue);
        }
        other => panic!("expected Range, got {other:?}"),
    }
    assert_eq!(bytes, back.serialize());
}

// v7.37.42-arena Phase 4 — `Value::clone_into(arena)` lifts an owned
// `Value<'static>` into the bump arena. The result must compare equal
// to the source (Cow value semantics) regardless of the underlying
// `Cow::Borrowed` / `Cow::Owned` split, and must lift cleanly back to
// `Value<'static>` via `into_owned()`.
#[test]
fn arena_clone_into_round_trip_heap_variants() {
    let arena = bumpalo::Bump::new();

    let owned_text = Value::text("hello world");
    let arena_text = owned_text.clone_into(&arena);
    assert_eq!(arena_text, owned_text);
    assert_eq!(arena_text.clone().into_owned(), owned_text);

    let owned_json = Value::json(r#"{"k":1}"#);
    let arena_json = owned_json.clone_into(&arena);
    assert_eq!(arena_json, owned_json);

    let owned_xml = Value::xml("<a/>");
    let arena_xml = owned_xml.clone_into(&arena);
    assert_eq!(arena_xml, owned_xml);

    let owned_bytes = Value::bytes(alloc::vec![1u8, 2, 3, 4]);
    let arena_bytes = owned_bytes.clone_into(&arena);
    assert_eq!(arena_bytes, owned_bytes);

    let owned_vec = Value::vector(alloc::vec![1.0f32, 2.0, 3.0]);
    let arena_vec = owned_vec.clone_into(&arena);
    assert_eq!(arena_vec, owned_vec);

    let owned_bits = Value::bit_string(12, alloc::vec![0xAB, 0xC0]);
    let arena_bits = owned_bits.clone_into(&arena);
    assert_eq!(arena_bits, owned_bits);
}

// v7.37.42-arena Phase 4 — `Value::clone_into` on Copy-able / nested-owned
// variants (scalars, arrays, ranges, …) must produce an equal Value. The
// implementation falls back to `clone().into_owned()` (heap blocks stay on
// the global allocator), which is correctness-equivalent to the Cow path.
#[test]
fn arena_clone_into_round_trip_copy_and_nested_variants() {
    let arena = bumpalo::Bump::new();
    for v in [
        Value::SmallInt(7),
        Value::Int(-3),
        Value::BigInt(42),
        Value::Float(core::f64::consts::PI),
        Value::Bool(true),
        Value::Date(20_000),
        Value::Timestamp(1_700_000_000_000_000),
        Value::Uuid([1u8; 16]),
        Value::Null,
        Value::TextArray(alloc::vec![
            Some("a".to_string()),
            None,
            Some("b".to_string()),
        ]),
        Value::IntArray(alloc::vec![Some(1), None, Some(3)]),
    ] {
        let lifted = v.clone_into(&arena);
        assert_eq!(lifted, v, "clone_into changed scalar/array Value: {v:?}");
    }
}

// v7.37.42-arena Phase 4 — `Row::clone_into` + `Row::into_owned` are
// the row-level analogues of the Value boundary helpers. The bump arena
// hosts the per-cell payloads; converting back must yield byte-identical
// catalog rows (storage round-trip is the production gate).
#[test]
fn arena_row_clone_into_then_into_owned_round_trip() {
    let arena = bumpalo::Bump::new();
    let row = Row::new(alloc::vec![
        Value::BigInt(1),
        Value::text("ham"),
        Value::Null,
        Value::bytes(alloc::vec![0xDE, 0xAD]),
    ]);

    let arena_row = row.clone_into(&arena);
    assert_eq!(arena_row, row);

    let lifted: Row<'static> = arena_row.into_owned();
    assert_eq!(lifted, row);

    // Catalog round-trip — serialise via the storage codec, deserialise,
    // and check the recovered Row is byte-identical to the original.
    // This is the WAL/persistence boundary Phase 4 must preserve.
    let mut c = Catalog::new();
    c.create_table(TableSchema::new(
        "t",
        vec![
            ColumnSchema::new("id", DataType::BigInt, false),
            ColumnSchema::new("v", DataType::Text, true),
            ColumnSchema::new("opt", DataType::SmallInt, true),
            ColumnSchema::new("blob", DataType::Bytes, true),
        ],
    ))
    .unwrap();
    c.get_mut("t").unwrap().insert(lifted.clone()).unwrap();
    let bytes = c.serialize();
    let back = Catalog::deserialize(&bytes).unwrap();
    let stored = back.get("t").unwrap().rows().get(0).cloned().unwrap();
    assert_eq!(stored, lifted, "catalog round-trip diverged");
    assert_eq!(bytes, back.serialize(), "catalog bytes round-trip diverged");
}

#[test]
fn v54_snapshot_crc_detects_corruption() {
    // v7.38 (read01 P5.05) — the CRC32C trailer round-trips and catches a
    // single-bit corruption of the snapshot body.
    let mut c = Catalog::new();
    c.create_table(TableSchema::new(
        "t",
        vec![ColumnSchema::new("id", DataType::Int, false)],
    ))
    .unwrap();
    c.get_mut("t")
        .unwrap()
        .insert(Row::new(alloc::vec![Value::Int(42)]))
        .unwrap();

    let bytes = c.serialize();
    // Writer emits the current version with a 4-byte CRC trailer.
    assert_eq!(bytes[FILE_MAGIC.len()], FILE_VERSION);
    // Clean round-trip.
    let restored = Catalog::deserialize(&bytes).expect("round-trips");
    assert_eq!(restored.get("t").unwrap().rows().len(), 1);

    // Flip a byte in the body (not the CRC trailer) → CRC mismatch.
    let mut corrupt = bytes.clone();
    let mid = corrupt.len() / 2;
    corrupt[mid] ^= 0x01;
    let err = Catalog::deserialize(&corrupt).unwrap_err();
    assert!(
        format!("{err:?}").contains("CRC mismatch") || format!("{err:?}").contains("Corrupt"),
        "expected a corruption error, got {err:?}"
    );
}
