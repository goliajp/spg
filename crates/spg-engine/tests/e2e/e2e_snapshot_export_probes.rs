//! v7.37.17 (17.6 siblings) — snapshot export/import + visibility
//! probes. Queue with v7.38 MVCC Phase C for real implementations.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn snapshot_export_probes_return_null() {
    let mut e = Engine::new();
    for f in &[
        "pg_export_snapshot()",
        "pg_snapshot()",
        "pg_import_snapshot('00000001-1-1')",
        "pg_import_serialized_snapshot('00000001-1-1')",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}

/// v7.39 (round 517) — this used to assert TRUE for
/// `pg_visible_in_snapshot(1, NULL)`, which was pinning SPG's own stub: the
/// function answered TRUE for everything, under a comment reading "no
/// MVCC-yet model". SPG has had MVCC since v7.37.15 and the stub outlived
/// it, so a caller probing a snapshot got "visible" for an id the snapshot
/// excludes.
///
/// PG18, measured: a NULL snapshot is NULL, and the real check is below
/// xmin visible, at or above xmax not, in between visible unless the id is
/// in the in-progress list.
#[test]
fn pg_visible_in_snapshot_reads_the_snapshot() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_visible_in_snapshot(1, NULL)"),
        spg_storage::Value::Null
    ));
    for (sql, want) in [
        ("SELECT pg_visible_in_snapshot(5, '10:20:')", true),
        ("SELECT pg_visible_in_snapshot(15, '10:20:')", true),
        ("SELECT pg_visible_in_snapshot(15, '10:20:15')", false),
        ("SELECT pg_visible_in_snapshot(25, '10:20:')", false),
    ] {
        match first(&mut e, sql) {
            spg_storage::Value::Bool(b) => assert_eq!(b, want, "{sql}"),
            other => panic!("{sql}: got {other:?}"),
        }
    }
}

#[test]
fn pg_last_xid_returns_bigint() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT pg_last_xid()") {
        spg_storage::Value::BigInt(_) => {}
        other => panic!("got {other:?}"),
    }
}
