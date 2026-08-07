//! read01 round 343 (V40) — the two large-object calls that touch a file.
//!
//! `lo_import('/path')` and `lo_export(oid, '/path')` were the last of the
//! lo_* family missing: the engine is `no_std` and has no filesystem, so
//! they need the host contract `COPY … FROM '<file>'` has used since round
//! 249 — the engine owns the shape and every message, each host supplies
//! only the `std::fs` call. That keeps the server and the embedded API
//! saying the same thing.
//!
//! PG 18.4, measured: the result column is named after the function,
//! `lo_import` answers the new oid and `lo_export` answers `1`; a missing
//! input is `could not open server file "…": No such file or directory`,
//! an unwritable target `could not create server file "…": …`, and a
//! missing object `large object N does not exist`.
//!
//! (Where the file IO happens is pinned by the host suites; this pins the
//! shape recognition and the wording, which both hosts share.)

use spg_engine::largeobject::{
    LoFileCall, could_not_create, could_not_open, parse_lo_file_call, permission_denied,
};

#[test]
fn the_two_file_calls_are_recognised() {
    assert_eq!(
        parse_lo_file_call("SELECT lo_import('/tmp/a.txt')"),
        Some(LoFileCall::Import {
            path: "/tmp/a.txt".into(),
            oid: None
        })
    );
    assert_eq!(
        parse_lo_file_call("SELECT lo_import('/tmp/a.txt', 4242)"),
        Some(LoFileCall::Import {
            path: "/tmp/a.txt".into(),
            oid: Some(4242)
        })
    );
    assert_eq!(
        parse_lo_file_call("SELECT lo_export(4242, '/tmp/b.bin')"),
        Some(LoFileCall::Export {
            oid: 4242,
            path: "/tmp/b.bin".into()
        })
    );
}

/// Narrow on purpose: only the statement-level spelling reaches a file.
/// Anything nested stays with the ordinary evaluator rather than being
/// half-executed by the host.
#[test]
fn nothing_else_is_intercepted() {
    for sql in [
        "SELECT lo_get(1)",
        "SELECT lo_import('/tmp/a') FROM t",
        "SELECT length(lo_import('/tmp/a'))",
        "INSERT INTO t VALUES (lo_import('/tmp/a'))",
        "SELECT 1",
    ] {
        assert_eq!(parse_lo_file_call(sql), None, "for `{sql}`");
    }
}

/// The column names are PG's — its own function names.
#[test]
fn the_result_column_is_named_after_the_function() {
    assert_eq!(
        parse_lo_file_call("SELECT lo_import('/tmp/a')")
            .unwrap()
            .column_name(),
        "lo_import"
    );
    assert_eq!(
        parse_lo_file_call("SELECT lo_export(1, '/tmp/a')")
            .unwrap()
            .column_name(),
        "lo_export"
    );
}

/// Every message is PG's, and std's `(os error N)` tail is not.
#[test]
fn the_messages_read_like_pg() {
    assert_eq!(
        could_not_open("/tmp/x", "No such file or directory (os error 2)"),
        "could not open server file \"/tmp/x\": No such file or directory",
    );
    assert_eq!(
        could_not_create("/nodir/x", "No such file or directory (os error 2)"),
        "could not create server file \"/nodir/x\": No such file or directory",
    );
    assert_eq!(
        permission_denied(&parse_lo_file_call("SELECT lo_import('/tmp/a')").unwrap()),
        "permission denied for function lo_import",
    );
}

/// An imported object is an ordinary large object afterwards, and its oid
/// no longer collides with the user-table band (a large object used to be
/// handed 16384 — the first table's oid — while
/// `pg_largeobject_metadata.oid` is joinable against `pg_class.oid`).
#[test]
fn an_imported_object_reads_back_through_the_family() {
    let mut e = spg_engine::Engine::new();
    let oid = e.lo_import_bytes(0, b"hello-lo\n".to_vec()).unwrap();
    assert!(oid >= 500_000, "large objects have their own band: {oid}");
    assert_eq!(e.lo_export_bytes(oid).unwrap(), b"hello-lo\n");

    e.execute("CREATE TABLE t (a INT)").unwrap();
    let table_oid = match e.execute("SELECT 't'::regclass::bigint").unwrap() {
        spg_engine::QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        other => panic!("{other:?}"),
    };
    assert_ne!(table_oid, spg_storage::Value::BigInt(i64::from(oid)));

    // …and lo_get sees exactly what was imported.
    let got = e
        .execute(&alloc_fmt(oid))
        .unwrap_or_else(|err| panic!("{err}"));
    match got {
        spg_engine::QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0].values[0], spg_storage::Value::Int(9));
        }
        other => panic!("{other:?}"),
    }
}

fn alloc_fmt(oid: u32) -> String {
    format!("SELECT length(lo_get({oid}))")
}

/// A missing object is PG's message, from the engine side.
#[test]
fn exporting_a_missing_object_is_pgs_error() {
    let e = spg_engine::Engine::new();
    let err = e.lo_export_bytes(99_999).unwrap_err();
    assert_eq!(
        format!("{err}"),
        "unsupported: large object 99999 does not exist"
    );
}
