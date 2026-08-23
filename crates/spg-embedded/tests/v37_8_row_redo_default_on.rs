// SAFETY: this is a standalone single-test binary so the env op
// can't race a concurrent reader thread (same argument the
// existing redo_recovery.rs uses). All three v7.37.8 assertions
// are folded into ONE test function so libtest can't schedule
// them in parallel and have one's env mutation race another's
// read.
#![allow(unsafe_code)]
//! v7.37.8 — `SPG_WAL_ROW_REDO` default flipped OFF → ON.
//!
//! **Why** (mailrs 4th-recurrence root cause): v7.34 P0 #2 shipped
//! the row-level redo path (0x13 records replay via apply_redo in
//! O(changed rows) instead of re-executing SQL in O(records ×
//! catalog_rows)). It shipped opt-in (`SPG_WAL_ROW_REDO=1`) "during
//! bringup". The dogfood "zero mailrs change" contract means mailrs
//! never set the env var; result = every container restart paid
//! the V4 SQL replay tax. Four cascade recurrences (06-16, 06-19,
//! 06-22 a, 06-22 b) all on the same root cause. v7.37.8 flips the
//! default ON so an `spg-X.Y.Z` upgrade alone delivers the fix.
//!
//! One test, three sub-assertions in series:
//! 1. Default (env unset) → row redo enabled (the post-upgrade
//!    contract).
//! 2. `SPG_WAL_ROW_REDO=0` → opt-out to legacy V4 SQL preserved.
//! 3. `SPG_WAL_ROW_REDO=1` → explicit opt-in still works.
//!
//! Three sub-assertions in one test function so libtest can't
//! parallelise them and have env mutations race assertion reads.
//! Differential coverage (V5 fast replay vs V4 slow replay)
//! continues to live in `lib.rs::tests::ask3_apply_redo_*`.

use spg_embedded::Database;

fn tmpdir(suffix: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let pid = std::process::id();
    let dir = std::env::temp_dir()
        .join("spg-tests")
        .join(format!("spg-v37_8-{suffix}-{pid}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn v37_8_row_redo_default_on_and_overrides() {
    // ===== 1. Default ON: env unset → row redo enabled =====
    // SAFETY: single-test binary, no concurrent env reader.
    unsafe {
        std::env::remove_var("SPG_WAL_ROW_REDO");
    }
    let dir = tmpdir("default");
    let db_path = dir.join("t.spg");
    {
        let db = Database::open_path(&db_path).expect("open default");
        assert!(
            db.engine().redo_capture_enabled(),
            "v7.37.8 default ON: engine redo_capture must be enabled \
             without SPG_WAL_ROW_REDO env set"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);

    // ===== 2. Operator opt-out: SPG_WAL_ROW_REDO=0 →
    //                            legacy V4 SQL preserved =====
    // SAFETY: see (1).
    unsafe {
        std::env::set_var("SPG_WAL_ROW_REDO", "0");
    }
    let dir = tmpdir("optout");
    let db_path = dir.join("t.spg");
    {
        let db = Database::open_path(&db_path).expect("open optout");
        assert!(
            !db.engine().redo_capture_enabled(),
            "SPG_WAL_ROW_REDO=0: engine redo_capture must be disabled \
             (operator opt-out to legacy V4 SQL)"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);

    // ===== 3. Explicit opt-in: SPG_WAL_ROW_REDO=1 → still ON =====
    // SAFETY: see (1).
    unsafe {
        std::env::set_var("SPG_WAL_ROW_REDO", "1");
    }
    let dir = tmpdir("optin");
    let db_path = dir.join("t.spg");
    {
        let db = Database::open_path(&db_path).expect("open optin");
        assert!(
            db.engine().redo_capture_enabled(),
            "SPG_WAL_ROW_REDO=1: engine redo_capture must remain enabled \
             (explicit opt-in still works)"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);

    // Clean up env for any subsequent process state.
    // SAFETY: see (1).
    unsafe {
        std::env::remove_var("SPG_WAL_ROW_REDO");
    }
}
