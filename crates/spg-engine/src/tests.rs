//! Engine unit tests split out of `lib.rs` (lib.rs split 8). The whole
//! `#[cfg(test)] mod tests` body moves here verbatim; `use super::*`
//! still resolves to the crate root, so every test sees the same items
//! as before. Grouped by theme via section comments for a future
//! per-topic split.

use super::*;
use alloc::string::ToString;
use alloc::vec;

use spg_sql::ast::{BinOp, Expr, Statement};
use spg_storage::{DataType, Value, VecEncoding};

fn unwrap_command_ok(r: &QueryResult) -> usize {
    match r {
        QueryResult::CommandOk { affected, .. } => *affected,
        QueryResult::Rows { .. } => panic!("expected CommandOk, got Rows"),
    }
}

#[test]
fn update_seek_positions_engages_on_indexed_eq() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE b (id INT NOT NULL, v INT NOT NULL)")
        .unwrap();
    e.execute("CREATE INDEX b_id ON b (id)").unwrap();
    for i in 0..100 {
        e.execute(&alloc::format!("INSERT INTO b VALUES ({i}, {i})"))
            .unwrap();
    }
    let stmt =
        spg_sql::parser::parse_statement("UPDATE b SET v = v + 1 WHERE id = 42").expect("parse");
    let Statement::Update(u) = stmt else {
        panic!("expected Update, got {stmt:?}");
    };
    let w = u.where_.as_ref().expect("where");
    let table = e.catalog().get("b").unwrap();
    let schema_cols = table.schema().columns.clone();
    // step-by-step: each sub-resolution must succeed.
    let Expr::Binary { lhs, op, rhs } = w else {
        panic!("WHERE not Binary: {w:?}");
    };
    assert_eq!(*op, BinOp::Eq, "op not Eq");
    let pair = resolve_col_literal_pair(lhs, rhs, &schema_cols, "b");
    assert!(
        pair.is_some(),
        "resolve_col_literal_pair None: lhs={lhs:?} rhs={rhs:?}"
    );
    let (col_pos, value) = pair.unwrap();
    assert!(
        table.index_on(col_pos).is_some(),
        "no index on col {col_pos}"
    );
    assert!(
        spg_storage::IndexKey::from_value(&value).is_some(),
        "IndexKey::from_value None for {value:?}"
    );
    // v7.39 (round 490) — the seek drops versions the snapshot cannot see,
    // so it needs one. `unbounded` accepts every header, which is what this
    // test's freshly-inserted rows are under.
    let snapshot = spg_storage::snapshot::Snapshot::unbounded();
    let positions = try_index_seek_positions(w, &schema_cols, table, "b", &snapshot);
    assert_eq!(positions, Some(vec![42]), "seek did not engage");
}

#[test]
fn create_table_registers_schema() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE foo (a INT NOT NULL, b TEXT)")
        .unwrap();
    assert_eq!(e.catalog().table_count(), 1);
    let t = e.catalog().get("foo").unwrap();
    assert_eq!(t.schema().columns.len(), 2);
    assert_eq!(t.schema().columns[0].ty, DataType::Int);
    assert!(!t.schema().columns[0].nullable);
    assert_eq!(t.schema().columns[1].ty, DataType::Text);
}

#[test]
fn create_table_vector_default_is_f32_encoded() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (v VECTOR(8))").unwrap();
    let t = e.catalog().get("t").unwrap();
    assert_eq!(
        t.schema().columns[0].ty,
        DataType::Vector {
            dim: 8,
            encoding: VecEncoding::F32,
        },
    );
}

#[test]
fn create_table_vector_using_sq8_succeeds() {
    // v6.0.1 step 3: the step-1 fence in `column_def_to_schema`
    // is lifted. CREATE TABLE persists an SQ8 column type in
    // the catalog; INSERT (next test) quantises raw f32 input.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (v VECTOR(8) USING SQ8)").unwrap();
    let t = e.catalog().get("t").unwrap();
    assert_eq!(
        t.schema().columns[0].ty,
        DataType::Vector {
            dim: 8,
            encoding: VecEncoding::Sq8,
        },
    );
}

#[test]
fn insert_into_sq8_column_quantises_f32_payload() {
    // v6.0.1 step 3: INSERT-time `coerce_value` rewrites a raw
    // `Value::vector(Vec<f32>)` literal into the column's
    // quantised representation. The row that lands in the
    // catalog must therefore hold a `Value::Sq8Vector`, not the
    // original f32 buffer — that's the bit that delivers the
    // 4× compression target.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (v VECTOR(4) USING SQ8)").unwrap();
    e.execute("INSERT INTO t VALUES ([0.0, 0.25, 0.5, 1.0])")
        .unwrap();
    let t = e.catalog().get("t").unwrap();
    assert_eq!(t.rows().len(), 1);
    match &t.rows()[0].values[0] {
        Value::Sq8Vector(q) => {
            assert_eq!(q.bytes.len(), 4);
            // min/max are derived from the payload: min=0.0, max=1.0.
            assert!((q.min - 0.0).abs() < 1e-6);
            assert!((q.max - 1.0).abs() < 1e-6);
        }
        other => panic!("expected Sq8Vector cell, got {other:?}"),
    }
}

#[test]
fn create_table_vector_using_half_succeeds_and_insert_converts_to_f16() {
    // v6.0.3: CREATE TABLE accepts USING HALF; INSERT path
    // converts the incoming `Value::vector(Vec<f32>)` cell
    // into `Value::HalfVector(HalfVector)` via the new
    // `coerce_value` arm. The dequantised round-trip is
    // bit-exact for f16-representable values, so 0.0 / 0.25
    // / 0.5 / 1.0 hit their grid points exactly.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (v VECTOR(4) USING HALF)")
        .unwrap();
    e.execute("INSERT INTO t VALUES ([0.0, 0.25, 0.5, 1.0])")
        .unwrap();
    let t = e.catalog().get("t").unwrap();
    assert_eq!(t.rows().len(), 1);
    match &t.rows()[0].values[0] {
        Value::HalfVector(h) => {
            assert_eq!(h.dim(), 4);
            let back = h.to_f32_vec();
            let expected = alloc::vec![0.0_f32, 0.25, 0.5, 1.0];
            for (g, e) in back.iter().zip(expected.iter()) {
                assert!(
                    (g - e).abs() < 1e-6,
                    "{g} vs {e} should be exact on f16 grid"
                );
            }
        }
        other => panic!("expected HalfVector cell, got {other:?}"),
    }
}

#[test]
fn alter_index_rebuild_in_place_succeeds() {
    // v6.0.4: bare REBUILD (no encoding switch) walks every
    // row again to rebuild the NSW graph. Verifies the engine
    // dispatch + storage helper plumbing without changing any
    // cell encoding.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, v VECTOR(3) NOT NULL)")
        .unwrap();
    for i in 0..8_i32 {
        #[allow(clippy::cast_precision_loss)]
        let base = (i as f32) * 0.1;
        e.execute(&alloc::format!(
            "INSERT INTO t VALUES ({i}, [{base}, {b1}, {b2}])",
            b1 = base + 0.01,
            b2 = base + 0.02,
        ))
        .unwrap();
    }
    e.execute("CREATE INDEX t_idx ON t USING hnsw (v)").unwrap();
    e.execute("ALTER INDEX t_idx REBUILD").unwrap();
    // Schema encoding stays F32 (no encoding clause).
    assert_eq!(
        e.catalog().get("t").unwrap().schema().columns[1].ty,
        DataType::Vector {
            dim: 3,
            encoding: VecEncoding::F32,
        },
    );
}

#[test]
fn alter_index_rebuild_with_encoding_switches_cell_type() {
    // v6.0.4: REBUILD WITH (encoding = SQ8) recodes every
    // stored cell from F32 → SQ8 + rebuilds the graph atop the
    // new encoding. Post-rebuild, cells must be Sq8Vector and
    // the schema must report encoding = Sq8.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, v VECTOR(4) NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, [0.0, 0.25, 0.5, 1.0])")
        .unwrap();
    e.execute("CREATE INDEX t_idx ON t USING hnsw (v)").unwrap();
    e.execute("ALTER INDEX t_idx REBUILD WITH (encoding = SQ8)")
        .unwrap();
    let t = e.catalog().get("t").unwrap();
    assert_eq!(
        t.schema().columns[1].ty,
        DataType::Vector {
            dim: 4,
            encoding: VecEncoding::Sq8,
        },
    );
    assert!(matches!(t.rows()[0].values[1], Value::Sq8Vector(_)));
}

#[test]
fn alter_index_rebuild_unknown_index_errors() {
    let mut e = Engine::new();
    let err = e.execute("ALTER INDEX nope REBUILD").unwrap_err();
    assert!(
        matches!(
            &err,
            EngineError::Storage(StorageError::IndexNotFound { name }) if name == "nope"
        ),
        "got: {err}"
    );
}

#[test]
fn alter_index_rebuild_on_btree_index_errors() {
    // REBUILD on a B-tree index has no semantic meaning in
    // v6.0.4 — rejected at the storage layer with `Unsupported`.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    e.execute("CREATE INDEX t_idx ON t (id)").unwrap();
    let err = e.execute("ALTER INDEX t_idx REBUILD").unwrap_err();
    assert!(
        matches!(&err, EngineError::Storage(StorageError::Unsupported(_))),
        "got: {err}"
    );
}

#[test]
fn prepared_insert_substitutes_placeholders() {
    // v6.1.1: prepare() parses once; execute_prepared() walks the
    // AST and replaces $1/$2 with the param Values BEFORE the
    // dispatch sees them. Same logical result as a simple-query
    // INSERT, but parse happens once per *statement*, not per
    // execution.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    let stmt = e.prepare("INSERT INTO t VALUES ($1, $2)").unwrap();
    for (id, name) in [(1, "alice"), (2, "bob"), (3, "carol")] {
        e.execute_prepared(
            stmt.clone(),
            &[Value::Int(id), Value::text::<String>(name.into())],
        )
        .unwrap();
    }
    // Read back via simple-query SELECT.
    let rows_result = e.execute("SELECT id, name FROM t").unwrap();
    let QueryResult::Rows { rows, .. } = rows_result else {
        panic!("expected Rows")
    };
    assert_eq!(rows.len(), 3);
}

#[test]
fn prepared_select_with_placeholder_filters_rows() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)")
        .unwrap();
    for i in 0..10_i32 {
        e.execute(&alloc::format!("INSERT INTO t VALUES ({i}, {})", i * 7))
            .unwrap();
    }
    let stmt = e.prepare("SELECT id FROM t WHERE v = $1").unwrap();
    let QueryResult::Rows { rows, .. } = e.execute_prepared(stmt, &[Value::Int(35)]).unwrap()
    else {
        panic!("expected Rows")
    };
    // v = 35 means i*7 = 35 → i = 5.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int(5));
}

#[test]
fn prepared_too_few_params_errors() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    let stmt = e.prepare("INSERT INTO t VALUES ($1)").unwrap();
    let err = e.execute_prepared(stmt, &[]).unwrap_err();
    assert!(
        matches!(
            &err,
            EngineError::Eval(EvalError::PlaceholderOutOfRange { n: 1, bound: 0 })
        ),
        "got: {err}"
    );
}

#[test]
fn bytea_cast_round_trips_text_input() {
    // v7.18 — `'hello'::bytea` produces the raw bytes. Closes
    // the mailrs D-pre #3 reverse-acceptance gap.
    let e = Engine::new();
    let r = e.execute_readonly("SELECT 'hello'::bytea").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::bytes(b"hello".to_vec()));
}

#[test]
fn bytea_cast_pg_escape_hex_form() {
    // E'\\xdeadbeef'::bytea — E-string decodes to `\xdeadbeef`
    // (literal 10 chars), then ::bytea reads it as PG hex
    // form bytea literal → 4 bytes.
    let e = Engine::new();
    let r = e.execute_readonly(r"SELECT E'\\xdeadbeef'::bytea").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows")
    };
    assert_eq!(
        rows[0].values[0],
        Value::bytes(vec![0xde, 0xad, 0xbe, 0xef])
    );
}

#[test]
fn bytea_cast_chains_through_octet_length() {
    // octet_length('hello'::bytea) → 5. Confirms the cast
    // composes inside larger expressions, not just at top
    // level.
    let e = Engine::new();
    let r = e
        .execute_readonly("SELECT octet_length('hello'::bytea)")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows")
    };
    match &rows[0].values[0] {
        Value::Int(n) => assert_eq!(*n, 5),
        Value::BigInt(n) => assert_eq!(*n, 5),
        other => panic!("expected integer length, got {other:?}"),
    }
}

#[test]
fn readonly_prepared_on_snapshot_select_with_placeholder() {
    // v7.18 — sqlx Pool fan-out relies on running prepared
    // SELECTs against a frozen snapshot without re-entering
    // the writer engine. Mirrors the simple-query SELECT path
    // in `execute_readonly_on_snapshot` but takes a Statement
    // + bound params (the shape sqlx's Execute path produces).
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, v INT NOT NULL)")
        .unwrap();
    for i in 0..10_i32 {
        e.execute(&alloc::format!("INSERT INTO t VALUES ({i}, {})", i * 7))
            .unwrap();
    }
    let snapshot = e.clone_snapshot();
    let stmt = e.prepare("SELECT id FROM t WHERE v = $1").unwrap();
    let QueryResult::Rows { rows, .. } =
        Engine::execute_readonly_prepared_on_snapshot(&snapshot, stmt, &[Value::Int(35)]).unwrap()
    else {
        panic!("expected Rows")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int(5));
}

#[test]
fn readonly_prepared_on_snapshot_rejects_writes() {
    // DDL / DML prepared statements on the readonly path must
    // surface `WriteRequired` so the spg-sqlx connection layer
    // routes them to the writer mutex instead of the snapshot.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    let snapshot = e.clone_snapshot();
    let stmt = e.prepare("INSERT INTO t VALUES ($1)").unwrap();
    let err = Engine::execute_readonly_prepared_on_snapshot(&snapshot, stmt, &[Value::Int(1)])
        .unwrap_err();
    assert!(matches!(&err, EngineError::WriteRequired), "got: {err}");
}

#[test]
fn readonly_prepared_on_snapshot_frozen_view() {
    // The snapshot reflects engine state at clone_snapshot()
    // time. Writes after the snapshot are NOT visible — caller
    // takes a fresh snapshot (or `AsyncReadHandle::refresh()`)
    // to see them. This is the contract the per-statement
    // refresh in spg-sqlx relies on.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    let snapshot = e.clone_snapshot();
    e.execute("INSERT INTO t VALUES (2)").unwrap();
    let stmt = e.prepare("SELECT id FROM t WHERE id = $1").unwrap();
    let QueryResult::Rows { rows, .. } =
        Engine::execute_readonly_prepared_on_snapshot(&snapshot, stmt, &[Value::Int(2)]).unwrap()
    else {
        panic!("expected Rows")
    };
    assert!(rows.is_empty(), "id=2 was inserted after snapshot");
}

#[test]
fn describe_prepared_on_snapshot_resolves_columns() {
    // v7.18 — sqlx's Executor::describe path on the readonly
    // fan-out needs to resolve column names + types against
    // the snapshot's catalog (not the live engine's catalog,
    // which may have moved on).
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    let snapshot = e.clone_snapshot();
    let stmt = e.prepare("SELECT id, name FROM t WHERE id = $1").unwrap();
    let (_params, cols) = Engine::describe_prepared_on_snapshot(&snapshot, &stmt);
    assert_eq!(cols.len(), 2);
    assert_eq!(cols[0].name, "id");
    assert_eq!(cols[0].ty, DataType::Int);
    assert_eq!(cols[1].name, "name");
    assert_eq!(cols[1].ty, DataType::Text);
}

#[test]
fn insert_into_half_column_dim_mismatch_errors() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (v VECTOR(4) USING HALF)")
        .unwrap();
    let err = e.execute("INSERT INTO t VALUES ([1.0, 2.0])").unwrap_err();
    assert!(matches!(
        &err,
        EngineError::Storage(StorageError::TypeMismatch { .. })
    ));
}

#[test]
fn insert_into_sq8_column_dim_mismatch_errors() {
    // Dim mismatch falls through the `coerce_value` Vector→Sq8
    // arm's guard and surfaces as `TypeMismatch` — the same
    // error the F32 path produces today, so client error
    // handling stays uniform across encodings.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (v VECTOR(4) USING SQ8)").unwrap();
    let err = e.execute("INSERT INTO t VALUES ([1.0, 2.0])").unwrap_err();
    assert!(
        matches!(
            &err,
            EngineError::Storage(StorageError::TypeMismatch { .. })
        ),
        "got: {err}",
    );
}

#[test]
fn create_table_duplicate_errors() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE foo (a INT)").unwrap();
    let err = e.execute("CREATE TABLE foo (a INT)").unwrap_err();
    assert!(matches!(
        err,
        EngineError::Storage(StorageError::DuplicateTable { ref name }) if name == "foo"
    ));
}

#[test]
fn insert_into_unknown_table_errors() {
    let mut e = Engine::new();
    let err = e.execute("INSERT INTO ghost VALUES (1)").unwrap_err();
    assert!(matches!(
        err,
        EngineError::Storage(StorageError::TableNotFound { ref name }) if name == "ghost"
    ));
}

#[test]
fn insert_happy_path_reports_one_affected() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE foo (a INT NOT NULL)").unwrap();
    let r = e.execute("INSERT INTO foo VALUES (42)").unwrap();
    assert_eq!(unwrap_command_ok(&r), 1);
    assert_eq!(e.catalog().get("foo").unwrap().row_count(), 1);
}

#[test]
fn insert_arity_mismatch_propagates() {
    // v7.38 (read01 sweep) — with no column list, supplying FEWER values than
    // columns is legal in PG (the trailing columns take DEFAULT / NULL); only
    // supplying MORE values than columns is an arity error.
    let mut e = Engine::new();
    e.execute("CREATE TABLE foo (a INT, b TEXT)").unwrap();
    // Fewer values: b (no default) becomes NULL.
    e.execute("INSERT INTO foo VALUES (1)").unwrap();
    match e.execute("SELECT a, b FROM foo").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0].values[0], spg_storage::Value::Int(1));
            assert_eq!(rows[0].values[1], spg_storage::Value::Null);
        }
        _ => panic!("expected rows"),
    }
    // More values than columns is still an arity error — v7.39 (round 88)
    // now carries PG's 42601 wording instead of the generic ArityMismatch.
    let err = e.execute("INSERT INTO foo VALUES (1, 'x', 3)").unwrap_err();
    assert!(
        err.to_string()
            .contains("INSERT has more expressions than target columns"),
        "got {err}"
    );
}

#[test]
fn insert_negative_integer_via_unary_minus() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE foo (a INT NOT NULL)").unwrap();
    e.execute("INSERT INTO foo VALUES (-7)").unwrap();
    let rows = e.catalog().get("foo").unwrap().rows();
    assert_eq!(rows[0].values[0], Value::Int(-7));
}

#[test]
fn insert_expression_evaluated_against_empty_context() {
    // PG-canonical: INSERT VALUES accepts an arbitrary scalar
    // expression. The engine evaluates against an empty row
    // context — column references would error, but pure
    // arithmetic / function calls are fine.
    let mut e = Engine::new();
    e.execute("CREATE TABLE foo (a INT NOT NULL)").unwrap();
    e.execute("INSERT INTO foo VALUES (1 + 2)").unwrap();
    let rows = e.catalog().get("foo").unwrap().rows();
    assert_eq!(rows[0].values[0], Value::Int(3));
}

#[test]
fn select_star_returns_all_rows_in_insertion_order() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE foo (a INT NOT NULL, b TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO foo VALUES (1, 'one')").unwrap();
    e.execute("INSERT INTO foo VALUES (2, 'two')").unwrap();
    e.execute("INSERT INTO foo VALUES (3, 'three')").unwrap();

    let r = e.execute("SELECT * FROM foo").unwrap();
    let QueryResult::Rows { columns, rows } = r else {
        panic!("expected Rows")
    };
    assert_eq!(columns.len(), 2);
    assert_eq!(columns[0].name, "a");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1].values, vec![Value::Int(2), Value::text("two")]);
}

#[test]
fn select_star_on_empty_table_returns_zero_rows() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE foo (a INT)").unwrap();
    let r = e.execute("SELECT * FROM foo").unwrap();
    match r {
        QueryResult::Rows { rows, .. } => assert!(rows.is_empty()),
        QueryResult::CommandOk { .. } => panic!("expected Rows"),
    }
}

// --- v0.4: WHERE + projection ------------------------------------------

fn make_three_row_users(e: &mut Engine) {
    e.execute("CREATE TABLE users (id INT NOT NULL, name TEXT NOT NULL, score INT)")
        .unwrap();
    e.execute("INSERT INTO users VALUES (1, 'alice', 90)")
        .unwrap();
    e.execute("INSERT INTO users VALUES (2, 'bob', NULL)")
        .unwrap();
    e.execute("INSERT INTO users VALUES (3, 'cara', 70)")
        .unwrap();
}

fn unwrap_rows(r: QueryResult) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    match r {
        QueryResult::Rows { columns, rows } => (columns, rows),
        QueryResult::CommandOk { .. } => panic!("expected Rows"),
    }
}

#[test]
fn where_filter_passes_only_true_rows() {
    let mut e = Engine::new();
    make_three_row_users(&mut e);
    let r = e.execute("SELECT * FROM users WHERE id > 1").unwrap();
    let (_, rows) = unwrap_rows(r);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values[0], Value::Int(2));
    assert_eq!(rows[1].values[0], Value::Int(3));
}

#[test]
fn where_with_null_result_filters_out_row() {
    let mut e = Engine::new();
    make_three_row_users(&mut e);
    // score is NULL for bob → score > 80 is NULL → row excluded
    let r = e.execute("SELECT * FROM users WHERE score > 80").unwrap();
    let (_, rows) = unwrap_rows(r);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[1], Value::text("alice"));
}

#[test]
fn projection_named_columns() {
    let mut e = Engine::new();
    make_three_row_users(&mut e);
    let r = e.execute("SELECT name, score FROM users").unwrap();
    let (cols, rows) = unwrap_rows(r);
    assert_eq!(cols.len(), 2);
    assert_eq!(cols[0].name, "name");
    assert_eq!(cols[1].name, "score");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].values, vec![Value::text("alice"), Value::Int(90)]);
}

#[test]
fn projection_with_column_alias() {
    let mut e = Engine::new();
    make_three_row_users(&mut e);
    let r = e
        .execute("SELECT name AS who FROM users WHERE id = 1")
        .unwrap();
    let (cols, rows) = unwrap_rows(r);
    assert_eq!(cols[0].name, "who");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::text("alice"));
}

#[test]
fn qualified_column_with_table_alias_resolves() {
    let mut e = Engine::new();
    make_three_row_users(&mut e);
    let r = e
        .execute("SELECT u.id, u.name FROM users AS u WHERE u.id < 3")
        .unwrap();
    let (cols, rows) = unwrap_rows(r);
    assert_eq!(cols.len(), 2);
    assert_eq!(rows.len(), 2);
}

#[test]
fn qualified_column_with_wrong_alias_errors() {
    let mut e = Engine::new();
    make_three_row_users(&mut e);
    let err = e.execute("SELECT x.id FROM users AS u").unwrap_err();
    assert!(matches!(
        err,
        EngineError::Eval(EvalError::UnknownQualifier { ref qualifier }) if qualifier == "x"
    ));
}

#[test]
fn select_unknown_column_errors_in_projection() {
    let mut e = Engine::new();
    make_three_row_users(&mut e);
    let err = e.execute("SELECT ghost FROM users").unwrap_err();
    assert!(matches!(
        err,
        EngineError::Eval(EvalError::ColumnNotFound { ref name }) if name == "ghost"
    ));
}

#[test]
fn where_unknown_column_errors() {
    let mut e = Engine::new();
    make_three_row_users(&mut e);
    let err = e
        .execute("SELECT * FROM users WHERE ghost = 1")
        .unwrap_err();
    assert!(matches!(
        err,
        EngineError::Eval(EvalError::ColumnNotFound { .. })
    ));
}

#[test]
fn expression_projection_evaluates_and_renders() {
    // Compound expressions in the SELECT list are evaluated per row;
    // the output column is typed TEXT, name defaults to the expression.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (3)").unwrap();
    let (_, rows) = unwrap_rows(e.execute("SELECT 1 + 2 FROM t").unwrap());
    assert_eq!(rows.len(), 1);
    // The expression evaluates to integer 3; rendered as the cell value
    // (storage::Value::Int(3) since arithmetic kept ints).
    assert_eq!(rows[0].values[0], Value::Int(3));
}

#[test]
fn select_unknown_table_errors() {
    let mut e = Engine::new();
    let err = e.execute("SELECT * FROM ghost").unwrap_err();
    assert!(matches!(
        err,
        EngineError::Storage(StorageError::TableNotFound { .. })
    ));
}

#[test]
fn invalid_sql_returns_parse_error() {
    // v4.4: UPDATE is now real SQL, so use a true syntactic
    // garbage payload for the parse-error path.
    let mut e = Engine::new();
    let err = e.execute("THIS_IS_NOT_A_KEYWORD foo bar baz").unwrap_err();
    assert!(matches!(err, EngineError::Parse(_)));
}

// --- v0.8 CREATE INDEX + index seek ------------------------------------

#[test]
fn create_index_registers_on_table() {
    let mut e = Engine::new();
    make_three_row_users(&mut e);
    e.execute("CREATE INDEX by_name ON users (name)").unwrap();
    let t = e.catalog().get("users").unwrap();
    assert_eq!(t.indices().len(), 1);
    assert_eq!(t.indices()[0].name, "by_name");
}

#[test]
fn create_index_on_unknown_table_errors() {
    let mut e = Engine::new();
    let err = e.execute("CREATE INDEX i ON ghost (a)").unwrap_err();
    assert!(matches!(
        err,
        EngineError::Storage(StorageError::TableNotFound { .. })
    ));
}

#[test]
fn create_index_on_unknown_column_errors() {
    let mut e = Engine::new();
    make_three_row_users(&mut e);
    let err = e.execute("CREATE INDEX i ON users (ghost)").unwrap_err();
    assert!(matches!(
        err,
        EngineError::Storage(StorageError::ColumnNotFound { .. })
    ));
}

#[test]
fn select_eq_uses_index_returns_same_rows_as_scan() {
    // Build two engines: one with an index, one without. Same query →
    // same row set (index is a planner optimisation, not a semantic
    // change).
    let mut without = Engine::new();
    make_three_row_users(&mut without);
    let mut with = Engine::new();
    make_three_row_users(&mut with);
    with.execute("CREATE INDEX by_id ON users (id)").unwrap();

    let q = "SELECT * FROM users WHERE id = 2";
    let (_, no_idx_rows) = unwrap_rows(without.execute(q).unwrap());
    let (_, idx_rows) = unwrap_rows(with.execute(q).unwrap());
    assert_eq!(no_idx_rows, idx_rows);
    assert_eq!(idx_rows.len(), 1);
}

#[test]
fn select_eq_with_no_matching_index_value_returns_empty() {
    let mut e = Engine::new();
    make_three_row_users(&mut e);
    e.execute("CREATE INDEX by_id ON users (id)").unwrap();
    let (_, rows) = unwrap_rows(e.execute("SELECT * FROM users WHERE id = 999").unwrap());
    assert_eq!(rows.len(), 0);
}

// --- v0.9 transactions -------------------------------------------------

#[test]
fn begin_sets_in_transaction_flag() {
    let mut e = Engine::new();
    assert!(!e.in_transaction());
    e.execute("BEGIN").unwrap();
    assert!(e.in_transaction());
}

// v7.39 (round 475) — this used to assert an ERROR, which was SPG's own
// answer rather than either oracle's, and it also left the transaction
// ABORTED so the whole block was lost. PG18 warns and treats the second
// BEGIN as a no-op; MariaDB 11 implicitly commits and starts a new one.
// Both measured live; `e2e_redundant_begin_and_gin_expr_round475` pins the
// rollback semantics that distinguish them.
#[test]
fn double_begin_warns_and_keeps_the_transaction() {
    let mut e = Engine::new();
    e.execute("BEGIN").unwrap();
    e.execute("BEGIN")
        .expect("a redundant BEGIN is a no-op in a PG session");
    assert!(e.in_transaction());
    // And the block is still usable, which the old ERROR path destroyed.
    e.execute("CREATE TABLE t (a INT)").unwrap();
    e.execute("ROLLBACK").unwrap();
}

// v7.39 (round 435) — these two used to assert an ERROR, which was SPG's
// own answer rather than either oracle's: PG18 replies `WARNING: there is
// no transaction in progress` and still reports COMMIT / ROLLBACK, and
// MariaDB 11 succeeds silently. Both measured live; the tests now pin that.
#[test]
fn commit_without_begin_is_a_no_op() {
    let mut e = Engine::new();
    e.execute("COMMIT").expect("bare COMMIT is a no-op");
    assert!(!e.in_transaction());
}

#[test]
fn rollback_without_begin_is_a_no_op() {
    let mut e = Engine::new();
    e.execute("ROLLBACK").expect("bare ROLLBACK is a no-op");
    assert!(!e.in_transaction());
}

#[test]
fn commit_applies_shadow_to_committed_catalog() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (v INT NOT NULL)").unwrap();
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    e.execute("INSERT INTO t VALUES (2)").unwrap();
    e.execute("COMMIT").unwrap();
    assert!(!e.in_transaction());
    assert_eq!(e.catalog().get("t").unwrap().row_count(), 2);
}

#[test]
fn rollback_discards_shadow() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (v INT NOT NULL)").unwrap();
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    e.execute("INSERT INTO t VALUES (2)").unwrap();
    e.execute("ROLLBACK").unwrap();
    assert!(!e.in_transaction());
    assert_eq!(e.catalog().get("t").unwrap().row_count(), 0);
}

#[test]
fn select_during_tx_sees_uncommitted_writes_own_session() {
    // The shadow catalog is read by SELECTs while a TX is open — the
    // session can see its own pending writes.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (v INT NOT NULL)").unwrap();
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO t VALUES (42)").unwrap();
    let (_, rows) = unwrap_rows(e.execute("SELECT * FROM t").unwrap());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int(42));
}

#[test]
fn snapshot_with_no_users_is_bare_catalog_format() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    let bytes = e.snapshot();
    assert_eq!(
        &bytes[..8],
        b"SPGDB001",
        "must be the bare v3.x catalog magic"
    );
    let e2 = Engine::restore_envelope(&bytes).unwrap();
    assert!(e2.users().is_empty());
    assert_eq!(e2.catalog().table_count(), 1);
}

#[test]
fn snapshot_with_users_round_trips_both_via_envelope() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.create_user("alice", "pw1", Role::Admin, [9; 16]).unwrap();
    e.create_user("bob", "pw2", Role::ReadOnly, [5; 16])
        .unwrap();
    let bytes = e.snapshot();
    assert_eq!(&bytes[..8], b"SPGENV01", "must be the v4.1 envelope magic");
    let e2 = Engine::restore_envelope(&bytes).unwrap();
    assert_eq!(e2.users().len(), 2);
    assert_eq!(e2.verify_user("alice", "pw1"), Some(Role::Admin));
    assert_eq!(e2.verify_user("bob", "pw2"), Some(Role::ReadOnly));
    assert_eq!(e2.verify_user("alice", "wrong"), None);
    assert_eq!(e2.catalog().table_count(), 1);
}

#[test]
fn ddl_inside_tx_also_rolled_back() {
    let mut e = Engine::new();
    e.execute("BEGIN").unwrap();
    e.execute("CREATE TABLE t (v INT)").unwrap();
    // Visible inside the TX.
    e.execute("SELECT * FROM t").unwrap();
    e.execute("ROLLBACK").unwrap();
    // Gone after rollback.
    let err = e.execute("SELECT * FROM t").unwrap_err();
    assert!(matches!(
        err,
        EngineError::Storage(StorageError::TableNotFound { .. })
    ));
}

// ── v6.1.2: CREATE / DROP PUBLICATION (engine-side) ──────

#[test]
fn create_publication_lands_in_catalog() {
    let mut e = Engine::new();
    assert!(e.publications().is_empty());
    e.execute("CREATE PUBLICATION pub_a").unwrap();
    assert_eq!(e.publications().len(), 1);
    assert!(e.publications().contains("pub_a"));
}

#[test]
fn create_publication_duplicate_errors() {
    let mut e = Engine::new();
    e.execute("CREATE PUBLICATION pub_a").unwrap();
    let err = e.execute("CREATE PUBLICATION pub_a").unwrap_err();
    assert!(
        alloc::format!("{err:?}").contains("DuplicateName"),
        "got {err:?}"
    );
}

#[test]
fn drop_publication_absent_refuses_if_exists_skips() {
    // v7.39 (round 754, F31-B4) — PG18-measured: the bare form
    // REFUSES (the old "PG-compatible silent no-op" pin asserted a
    // behaviour PG does not have); IF EXISTS skips with affected=0.
    let mut e = Engine::new();
    let err = e.execute("DROP PUBLICATION nope").unwrap_err();
    assert!(
        alloc::format!("{err}").contains("publication \"nope\" does not exist"),
        "got {err}"
    );
    let r = e.execute("DROP PUBLICATION IF EXISTS nope").unwrap();
    match r {
        QueryResult::CommandOk { affected, .. } => assert_eq!(affected, 0),
        other => panic!("expected CommandOk, got {other:?}"),
    }
}

#[test]
fn drop_publication_present_reports_one_affected() {
    let mut e = Engine::new();
    e.execute("CREATE PUBLICATION pub_a").unwrap();
    let r = e.execute("DROP PUBLICATION pub_a").unwrap();
    match r {
        QueryResult::CommandOk {
            affected,
            modified_catalog,
        } => {
            assert_eq!(affected, 1);
            assert!(modified_catalog);
        }
        other => panic!("expected CommandOk, got {other:?}"),
    }
    assert!(e.publications().is_empty());
}

#[test]
fn publications_persist_across_snapshot_restore() {
    // The persist-across-restart ship-gate at the engine layer —
    // snapshot → restore_envelope round trip must preserve the
    // publication catalog. The spg-server e2e covers the
    // process-restart variant.
    let mut e = Engine::new();
    e.execute("CREATE PUBLICATION pub_a").unwrap();
    e.execute("CREATE PUBLICATION pub_b FOR ALL TABLES")
        .unwrap();
    let snap = e.snapshot();
    let e2 = Engine::restore_envelope(&snap).unwrap();
    assert_eq!(e2.publications().len(), 2);
    assert!(e2.publications().contains("pub_a"));
    assert!(e2.publications().contains("pub_b"));
}

#[test]
fn create_publication_allowed_inside_transaction() {
    // v6.1.4 dropped the v6.1.2 in-TX guard — PG allows
    // CREATE PUBLICATION inside a TX and the auto-commit
    // wrap path needs the same allowance.
    let mut e = Engine::new();
    e.execute("BEGIN").unwrap();
    e.execute("CREATE PUBLICATION pub_a").unwrap();
    e.execute("COMMIT").unwrap();
    assert!(e.publications().contains("pub_a"));
}

// ── v6.1.3: SHOW PUBLICATIONS + FOR-list variants ───────

#[test]
fn create_publication_for_table_list_lands_with_scope() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t1 (id INT NOT NULL)").unwrap();
    e.execute("CREATE TABLE t2 (id INT NOT NULL)").unwrap();
    e.execute("CREATE PUBLICATION pub_a FOR TABLE t1, t2")
        .unwrap();
    let scope = e.publications().get("pub_a").cloned();
    let Some(spg_sql::ast::PublicationScope::ForTables(ts)) = scope else {
        panic!("expected ForTables scope, got {scope:?}")
    };
    assert_eq!(ts, alloc::vec!["t1".to_string(), "t2".to_string()]);
}

#[test]
fn create_publication_all_tables_except_lands_with_scope() {
    let mut e = Engine::new();
    // v7.39 (round 754) — listed relations must exist now.
    e.execute("CREATE TABLE t3 (id INT NOT NULL)").unwrap();
    e.execute("CREATE PUBLICATION pub_a FOR ALL TABLES EXCEPT t3")
        .unwrap();
    let scope = e.publications().get("pub_a").cloned();
    let Some(spg_sql::ast::PublicationScope::AllTablesExcept(ts)) = scope else {
        panic!("expected AllTablesExcept scope, got {scope:?}")
    };
    assert_eq!(ts, alloc::vec!["t3".to_string()]);
}

#[test]
fn show_publications_empty_returns_zero_rows() {
    let e = Engine::new();
    let r = e.execute_readonly("SHOW PUBLICATIONS").unwrap();
    let QueryResult::Rows { rows, columns } = r else {
        panic!()
    };
    assert!(rows.is_empty());
    assert_eq!(columns.len(), 3);
    assert_eq!(columns[0].name, "name");
    assert_eq!(columns[1].name, "scope");
    assert_eq!(columns[2].name, "table_count");
}

#[test]
fn show_publications_returns_one_row_per_publication_ordered_by_name() {
    let mut e = Engine::new();
    // v7.39 (round 754) — listed relations must exist now.
    e.execute("CREATE TABLE t1 (id INT NOT NULL)").unwrap();
    e.execute("CREATE TABLE t2 (id INT NOT NULL)").unwrap();
    e.execute("CREATE TABLE bad (id INT NOT NULL)").unwrap();
    e.execute("CREATE PUBLICATION z_pub").unwrap();
    e.execute("CREATE PUBLICATION a_pub FOR TABLE t1, t2")
        .unwrap();
    e.execute("CREATE PUBLICATION m_pub FOR ALL TABLES EXCEPT bad")
        .unwrap();
    let r = e.execute_readonly("SHOW PUBLICATIONS").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 3);
    // Alphabetical order: a_pub, m_pub, z_pub.
    let names: Vec<&str> = rows
        .iter()
        .map(|r| {
            if let Value::Text(s) = &r.values[0] {
                s.as_ref()
            } else {
                panic!()
            }
        })
        .collect();
    assert_eq!(names, alloc::vec!["a_pub", "m_pub", "z_pub"]);
    // Row 0 — a_pub scope summary + table_count = 2.
    match &rows[0].values[1] {
        Value::Text(s) => assert_eq!(s, "FOR TABLE t1, t2"),
        other => panic!("expected Text, got {other:?}"),
    }
    assert_eq!(rows[0].values[2], Value::Int(2));
    // Row 1 — m_pub.
    match &rows[1].values[1] {
        Value::Text(s) => assert_eq!(s, "FOR ALL TABLES EXCEPT bad"),
        other => panic!("expected Text, got {other:?}"),
    }
    assert_eq!(rows[1].values[2], Value::Int(1));
    // Row 2 — z_pub (AllTables → NULL count).
    match &rows[2].values[1] {
        Value::Text(s) => assert_eq!(s, "FOR ALL TABLES"),
        other => panic!("expected Text, got {other:?}"),
    }
    assert_eq!(rows[2].values[2], Value::Null);
}

#[test]
fn for_list_scopes_persist_across_snapshot() {
    // The v6.1.2 envelope-v3 round-trip exercised AllTables;
    // v6.1.3 needs the scope-1 / scope-2 tags to survive too.
    let mut e = Engine::new();
    // v7.39 (round 754) — listed relations must exist now.
    for t in ["t1", "t2", "bad", "worse"] {
        e.execute(&alloc::format!("CREATE TABLE {t} (id INT NOT NULL)"))
            .unwrap();
    }
    e.execute("CREATE PUBLICATION p1 FOR TABLE t1, t2").unwrap();
    e.execute("CREATE PUBLICATION p2 FOR ALL TABLES EXCEPT bad, worse")
        .unwrap();
    let snap = e.snapshot();
    let e2 = Engine::restore_envelope(&snap).unwrap();
    assert_eq!(e2.publications().len(), 2);
    let p1 = e2.publications().get("p1").cloned();
    let Some(spg_sql::ast::PublicationScope::ForTables(ts)) = p1 else {
        panic!("p1 scope lost: {p1:?}")
    };
    assert_eq!(ts, alloc::vec!["t1".to_string(), "t2".to_string()]);
    let p2 = e2.publications().get("p2").cloned();
    let Some(spg_sql::ast::PublicationScope::AllTablesExcept(ts)) = p2 else {
        panic!("p2 scope lost: {p2:?}")
    };
    assert_eq!(ts, alloc::vec!["bad".to_string(), "worse".to_string()]);
}

// ── v6.1.4: CREATE / DROP SUBSCRIPTION + SHOW + envelope v4 ─

#[test]
fn create_subscription_lands_in_catalog_with_defaults() {
    let mut e = Engine::new();
    e.execute("CREATE SUBSCRIPTION sub_a CONNECTION 'host=127.0.0.1 port=20002' PUBLICATION pub_a")
        .unwrap();
    let s = e.subscriptions().get("sub_a").cloned().expect("present");
    assert_eq!(s.conn_str, "host=127.0.0.1 port=20002");
    assert_eq!(s.publications, alloc::vec!["pub_a".to_string()]);
    assert!(s.enabled);
    assert_eq!(s.last_received_pos, 0);
}

#[test]
fn create_subscription_duplicate_name_errors() {
    let mut e = Engine::new();
    e.execute("CREATE SUBSCRIPTION s CONNECTION 'host=x' PUBLICATION p")
        .unwrap();
    let err = e
        .execute("CREATE SUBSCRIPTION s CONNECTION 'host=y' PUBLICATION p")
        .unwrap_err();
    assert!(
        alloc::format!("{err:?}").contains("DuplicateName"),
        "got {err:?}"
    );
}

#[test]
fn drop_subscription_absent_refuses_if_exists_skips() {
    // v7.39 (round 754, F31-B4) — same contract as publications:
    // bare refuses with PG's sentence, IF EXISTS skips.
    let mut e = Engine::new();
    let err = e.execute("DROP SUBSCRIPTION never").unwrap_err();
    assert!(
        alloc::format!("{err}").contains("subscription \"never\" does not exist"),
        "got {err}"
    );
    let r = e.execute("DROP SUBSCRIPTION IF EXISTS never").unwrap();
    match r {
        QueryResult::CommandOk { affected, .. } => assert_eq!(affected, 0),
        other => panic!("expected CommandOk, got {other:?}"),
    }
}

#[test]
fn subscription_advance_updates_last_pos_monotone() {
    let mut e = Engine::new();
    e.execute("CREATE SUBSCRIPTION s CONNECTION 'h=x' PUBLICATION p")
        .unwrap();
    assert!(e.subscription_advance("s", 100));
    assert_eq!(e.subscriptions().get("s").unwrap().last_received_pos, 100);
    assert!(e.subscription_advance("s", 50)); // stale → ignored
    assert_eq!(e.subscriptions().get("s").unwrap().last_received_pos, 100);
    assert!(e.subscription_advance("s", 200));
    assert_eq!(e.subscriptions().get("s").unwrap().last_received_pos, 200);
    assert!(!e.subscription_advance("missing", 1));
}

#[test]
fn show_subscriptions_returns_rows_ordered_by_name() {
    let mut e = Engine::new();
    e.execute("CREATE SUBSCRIPTION z_sub CONNECTION 'h=x' PUBLICATION p1, p2")
        .unwrap();
    e.execute("CREATE SUBSCRIPTION a_sub CONNECTION 'h=y' PUBLICATION p3")
        .unwrap();
    let r = e.execute_readonly("SHOW SUBSCRIPTIONS").unwrap();
    let QueryResult::Rows { rows, columns } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(columns.len(), 5);
    assert_eq!(columns[0].name, "name");
    assert_eq!(columns[4].name, "last_received_pos");
    // Alphabetical: a_sub, z_sub.
    let names: Vec<&str> = rows
        .iter()
        .map(|r| {
            if let Value::Text(s) = &r.values[0] {
                s.as_ref()
            } else {
                panic!()
            }
        })
        .collect();
    assert_eq!(names, alloc::vec!["a_sub", "z_sub"]);
    // Row 0: a_sub
    assert_eq!(rows[0].values[1], Value::text("h=y".to_string()));
    assert_eq!(rows[0].values[2], Value::text("p3".to_string()));
    assert_eq!(rows[0].values[3], Value::Bool(true));
    assert_eq!(rows[0].values[4], Value::BigInt(0));
    // Row 1: z_sub — publications join with ", "
    assert_eq!(rows[1].values[2], Value::text("p1, p2".to_string()));
}

#[test]
fn subscriptions_persist_across_snapshot_envelope_v4() {
    let mut e = Engine::new();
    e.execute("CREATE SUBSCRIPTION s1 CONNECTION 'h=A' PUBLICATION p1, p2")
        .unwrap();
    e.execute("CREATE SUBSCRIPTION s2 CONNECTION 'h=B' PUBLICATION p3")
        .unwrap();
    e.subscription_advance("s2", 42);
    let snap = e.snapshot();
    let e2 = Engine::restore_envelope(&snap).unwrap();
    assert_eq!(e2.subscriptions().len(), 2);
    let s1 = e2.subscriptions().get("s1").unwrap();
    assert_eq!(s1.conn_str, "h=A");
    assert_eq!(
        s1.publications,
        alloc::vec!["p1".to_string(), "p2".to_string()]
    );
    assert_eq!(s1.last_received_pos, 0);
    let s2 = e2.subscriptions().get("s2").unwrap();
    assert_eq!(s2.last_received_pos, 42);
}

#[test]
fn v3_envelope_loads_with_empty_subscriptions() {
    // v3 snapshot (publications-only). Forge it by hand so we
    // verify v6.1.4 readers don't panic — they must surface
    // empty subscriptions and a populated publication table.
    let mut e = Engine::new();
    e.execute("CREATE PUBLICATION pub_legacy").unwrap();
    let catalog = e.catalog.serialize();
    let users = crate::users::serialize_users(&e.users);
    let pubs = e.publications.serialize();
    let mut buf = Vec::new();
    buf.extend_from_slice(b"SPGENV01");
    buf.push(3u8); // v3
    buf.extend_from_slice(&u32::try_from(catalog.len()).unwrap().to_le_bytes());
    buf.extend_from_slice(&catalog);
    buf.extend_from_slice(&u32::try_from(users.len()).unwrap().to_le_bytes());
    buf.extend_from_slice(&users);
    buf.extend_from_slice(&u32::try_from(pubs.len()).unwrap().to_le_bytes());
    buf.extend_from_slice(&pubs);
    let crc = spg_crypto::crc32::crc32(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());

    let e2 = Engine::restore_envelope(&buf).expect("v3 envelope restores under v4 reader");
    assert!(e2.subscriptions().is_empty());
    assert!(e2.publications().contains("pub_legacy"));
}

#[test]
fn create_subscription_allowed_inside_transaction() {
    let mut e = Engine::new();
    e.execute("BEGIN").unwrap();
    e.execute("CREATE SUBSCRIPTION s CONNECTION 'h=x' PUBLICATION p")
        .unwrap();
    e.execute("COMMIT").unwrap();
    assert!(e.subscriptions().contains("s"));
}

// ── v6.2.0: ANALYZE + spg_statistic + envelope v5 ──────────
#[test]
fn analyze_populates_histogram_bounds() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)")
        .unwrap();
    for i in 0..50 {
        e.execute(&alloc::format!("INSERT INTO t VALUES ({i}, 'name{i}')"))
            .unwrap();
    }
    e.execute("ANALYZE t").unwrap();
    let stats = e.statistics();
    let id_stats = stats.get("t", "id").unwrap();
    assert!(id_stats.histogram_bounds.len() >= 2);
    assert_eq!(id_stats.histogram_bounds.first().unwrap(), "0");
    assert_eq!(id_stats.histogram_bounds.last().unwrap(), "49");
    assert!((id_stats.null_frac - 0.0).abs() < 1e-6);
    assert_eq!(id_stats.n_distinct, 50);
}

#[test]
fn reanalyze_overwrites_prior_stats() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    for i in 0..10 {
        e.execute(&alloc::format!("INSERT INTO t VALUES ({i})"))
            .unwrap();
    }
    e.execute("ANALYZE t").unwrap();
    let n1 = e.statistics().get("t", "id").unwrap().n_distinct;
    assert_eq!(n1, 10);
    for i in 10..30 {
        e.execute(&alloc::format!("INSERT INTO t VALUES ({i})"))
            .unwrap();
    }
    e.execute("ANALYZE t").unwrap();
    let n2 = e.statistics().get("t", "id").unwrap().n_distinct;
    assert_eq!(n2, 30);
}

#[test]
fn analyze_unknown_table_errors() {
    let mut e = Engine::new();
    let err = e.execute("ANALYZE nonexistent").unwrap_err();
    assert!(matches!(
        err,
        EngineError::Storage(StorageError::TableNotFound { .. })
    ));
}

#[test]
fn bare_analyze_covers_all_user_tables() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t1 (id INT NOT NULL)").unwrap();
    e.execute("CREATE TABLE t2 (name TEXT NOT NULL)").unwrap();
    e.execute("INSERT INTO t1 VALUES (1)").unwrap();
    e.execute("INSERT INTO t2 VALUES ('alice')").unwrap();
    let r = e.execute("ANALYZE").unwrap();
    match r {
        QueryResult::CommandOk {
            affected,
            modified_catalog,
        } => {
            assert_eq!(affected, 2);
            assert!(modified_catalog);
        }
        other => panic!("expected CommandOk, got {other:?}"),
    }
    assert!(e.statistics().get("t1", "id").is_some());
    assert!(e.statistics().get("t2", "name").is_some());
}

#[test]
fn select_from_spg_statistic_returns_rows_per_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, label TEXT)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
    e.execute("INSERT INTO t VALUES (2, 'b')").unwrap();
    e.execute("ANALYZE t").unwrap();
    let r = e.execute_readonly("SELECT * FROM spg_statistic").unwrap();
    let QueryResult::Rows { rows, columns } = r else {
        panic!()
    };
    // v6.7.0 — spg_statistic gained a `cold_row_count` column.
    assert_eq!(columns.len(), 6);
    assert_eq!(columns[0].name, "table_name");
    assert_eq!(columns[4].name, "histogram_bounds");
    assert_eq!(columns[5].name, "cold_row_count");
    assert_eq!(rows.len(), 2, "one row per column of t");
    // Sorted by (table_name, column_name).
    match (&rows[0].values[0], &rows[0].values[1]) {
        (Value::Text(t), Value::Text(c)) => {
            assert_eq!(t, "t");
            // BTreeMap orders (table, column); columns "id" < "label".
            assert_eq!(c, "id");
        }
        _ => panic!(),
    }
}

#[test]
fn analyze_skips_vector_columns() {
    // Vector columns have their own stats shape (HNSW graph);
    // ANALYZE leaves them out of spg_statistic.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, v VECTOR(3) NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, [1, 2, 3])").unwrap();
    e.execute("ANALYZE t").unwrap();
    assert!(e.statistics().get("t", "id").is_some());
    assert!(e.statistics().get("t", "v").is_none());
}

#[test]
fn statistics_persist_across_envelope_v5_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    for i in 0..20 {
        e.execute(&alloc::format!("INSERT INTO t VALUES ({i})"))
            .unwrap();
    }
    e.execute("ANALYZE").unwrap();
    let snap = e.snapshot();
    let e2 = Engine::restore_envelope(&snap).unwrap();
    let s = e2.statistics().get("t", "id").unwrap();
    assert_eq!(s.n_distinct, 20);
}

// ── v6.2.1 auto-analyze threshold ───────────────────────────

#[test]
fn auto_analyze_threshold_honours_pg_50_row_base_on_small_table() {
    // v7.38 (read01 P5.29) — PG threshold = 50 + 0.1 × reltuples. A small
    // table therefore needs to cross the 50-row base before it re-analyzes,
    // not just 10% of a 100-row floor. After N inserts modified = row_count =
    // N, so it fires once N >= 50 + ceil(N/10), i.e. at N = 56.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    for i in 0..50 {
        e.execute(&alloc::format!("INSERT INTO t VALUES ({i})"))
            .unwrap();
    }
    // 50 mods vs threshold 50 + ceil(50/10) = 55 → no analyze yet.
    assert!(
        e.tables_needing_analyze().is_empty(),
        "50 < base+scale threshold"
    );
    for i in 50..60 {
        e.execute(&alloc::format!("INSERT INTO t VALUES ({i})"))
            .unwrap();
    }
    // 60 mods vs threshold 50 + ceil(60/10) = 56 → fires.
    assert_eq!(e.tables_needing_analyze(), alloc::vec!["t".to_string()]);
}

#[test]
fn auto_analyze_threshold_uses_10pct_of_row_count_for_large_tables() {
    // After ANALYZE on 1000 rows, threshold = 0.1 × row_count.
    // Each new INSERT bumps both modified and row_count, so to
    // trigger from N=1000 we need modifications ≥ 0.1 × (1000+M),
    // i.e. M ≥ 112. The test inserts 50 (no fire), then 150
    // more (200 total mods, row_count=1200, threshold=120 → fire).
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    for i in 0..1000 {
        e.execute(&alloc::format!("INSERT INTO t VALUES ({i})"))
            .unwrap();
    }
    e.execute("ANALYZE t").unwrap();
    assert!(e.tables_needing_analyze().is_empty(), "fresh ANALYZE");
    for i in 1000..1050 {
        e.execute(&alloc::format!("INSERT INTO t VALUES ({i})"))
            .unwrap();
    }
    assert!(
        e.tables_needing_analyze().is_empty(),
        "50 inserts < threshold of ~105"
    );
    for i in 1050..1200 {
        e.execute(&alloc::format!("INSERT INTO t VALUES ({i})"))
            .unwrap();
    }
    assert_eq!(
        e.tables_needing_analyze(),
        alloc::vec!["t".to_string()],
        "200 inserts > 0.1 × 1200 threshold"
    );
}

#[test]
fn auto_analyze_threshold_resets_after_analyze() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    for i in 0..200 {
        e.execute(&alloc::format!("INSERT INTO t VALUES ({i})"))
            .unwrap();
    }
    assert!(!e.tables_needing_analyze().is_empty());
    e.execute("ANALYZE").unwrap();
    assert!(
        e.tables_needing_analyze().is_empty(),
        "ANALYZE must reset the counter"
    );
}

#[test]
fn auto_analyze_threshold_tracks_updates_and_deletes() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, label TEXT)")
        .unwrap();
    for i in 0..200 {
        e.execute(&alloc::format!("INSERT INTO t VALUES ({i}, 'x')"))
            .unwrap();
    }
    e.execute("ANALYZE t").unwrap();
    // v7.38 (read01 P5.29) — UPDATE + DELETE both count toward n_mod. With
    // 200 rows the threshold is 50 + ceil(200/10) = 70; UPDATE 60 + DELETE 20
    // = 80 modifications > 70 → fires (and confirms updates/deletes count).
    e.execute("UPDATE t SET label = 'y' WHERE id < 60").unwrap();
    e.execute("DELETE FROM t WHERE id >= 180").unwrap();
    assert_eq!(e.tables_needing_analyze(), alloc::vec!["t".to_string()]);
}

#[test]
fn v4_envelope_loads_with_empty_statistics() {
    // Forge a v4 envelope by hand: catalog + users + pubs +
    // subs trailer, no statistics. A v6.2.0 reader must accept
    // it and surface an empty Statistics.
    let mut e = Engine::new();
    e.create_user("alice", "secret", crate::users::Role::ReadOnly, [0u8; 16])
        .unwrap();
    let catalog = e.catalog.serialize();
    let users = crate::users::serialize_users(&e.users);
    let pubs = e.publications.serialize();
    let subs = e.subscriptions.serialize();
    let mut buf = Vec::new();
    buf.extend_from_slice(b"SPGENV01");
    buf.push(4u8);
    buf.extend_from_slice(&u32::try_from(catalog.len()).unwrap().to_le_bytes());
    buf.extend_from_slice(&catalog);
    buf.extend_from_slice(&u32::try_from(users.len()).unwrap().to_le_bytes());
    buf.extend_from_slice(&users);
    buf.extend_from_slice(&u32::try_from(pubs.len()).unwrap().to_le_bytes());
    buf.extend_from_slice(&pubs);
    buf.extend_from_slice(&u32::try_from(subs.len()).unwrap().to_le_bytes());
    buf.extend_from_slice(&subs);
    let crc = spg_crypto::crc32::crc32(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());
    let e2 = Engine::restore_envelope(&buf).expect("v4 envelope restores");
    assert!(e2.statistics().is_empty());
}

#[test]
fn v1_v2_envelope_loads_with_empty_publications() {
    // A snapshot taken before v6.1.2 (no publication trailer,
    // envelope v2) must still deserialise — and the resulting
    // engine must report zero publications. Use the engine's own
    // round-trip with no publications: that emits v3 but with an
    // empty pubs block. Then forge a v2 envelope by hand to lock
    // the back-compat path.
    let mut e = Engine::new();
    // Force users to be non-empty so the snapshot takes the
    // envelope path rather than the bare-catalog fallback.
    e.create_user("alice", "secret", crate::users::Role::ReadOnly, [0u8; 16])
        .unwrap();

    // Forge an envelope v2: same shape as v3 but no pubs trailer.
    let catalog = e.catalog.serialize();
    let users = crate::users::serialize_users(&e.users);
    let mut buf = Vec::new();
    buf.extend_from_slice(b"SPGENV01");
    buf.push(2u8); // v2
    buf.extend_from_slice(&u32::try_from(catalog.len()).unwrap().to_le_bytes());
    buf.extend_from_slice(&catalog);
    buf.extend_from_slice(&u32::try_from(users.len()).unwrap().to_le_bytes());
    buf.extend_from_slice(&users);
    let crc = spg_crypto::crc32::crc32(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());

    let e2 = Engine::restore_envelope(&buf).expect("v2 envelope restores");
    assert!(e2.publications().is_empty());
}

// v7.38 P0 元机制 A — SQL-facing roundtrip. Verifies that
// `SELECT spg_injection_attach(...)` finds the per-engine store
// via the thread-local scope set up by `execute_*`, and that a
// subsequent `injection_point!` hit at one of the registered
// sites records / blocks per the attached action.
#[cfg(feature = "injection-points")]
#[test]
fn sql_injection_attach_notice_then_trigger_records() {
    use crate::QueryResult;
    let mut e = crate::Engine::new();
    // Attach a `notice` action via SQL — exercises
    // `eval::spg_injection_attach`, which is the test driver's
    // sole hook for plumbing actions in.
    let r = e
        .execute("SELECT spg_injection_attach('aggregate_spill_trigger', 'notice:from_sql')")
        .expect("attach succeeds");
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].values[0], Value::Bool(true));
        }
        other => panic!("expected Rows, got {other:?}"),
    }

    // Trigger the `aggregate_spill_trigger` point by running a
    // GROUP BY query — `aggregate::run` fires the point at its
    // entry.
    e.execute("CREATE TABLE inj_t (a INT)").unwrap();
    e.execute("INSERT INTO inj_t VALUES (1), (2), (1)").unwrap();
    e.execute("SELECT a, COUNT(*) FROM inj_t GROUP BY a")
        .unwrap();

    // The notice action records into the per-engine store. The
    // store is shared via Arc, so reading from the engine handle
    // here sees the same tally the trigger updated.
    let store = e.injection_store();
    assert!(
        store.notice_count("aggregate_spill_trigger") >= 1,
        "aggregate inject point did not fire under feature-on build"
    );
    assert_eq!(
        store.notice_message("aggregate_spill_trigger").as_deref(),
        Some("from_sql")
    );
}

// v7.38 P0 元机制 A — per-site testcase: planner_first_row_fetch.
// Any `SELECT` exercises `exec_select_cancel` which fires this
// point at its head. Most basic inject-site coverage shape.
#[cfg(feature = "injection-points")]
#[test]
fn inject_planner_first_row_fetch_fires_on_any_select() {
    let mut e = crate::Engine::new();
    e.execute("SELECT spg_injection_attach('planner_first_row_fetch', 'notice:select_seen')")
        .expect("attach succeeds");
    e.execute("CREATE TABLE inj_pfrf (id INT)").unwrap();
    e.execute("INSERT INTO inj_pfrf VALUES (1), (2)").unwrap();
    // Any SELECT triggers the head injection point.
    e.execute("SELECT * FROM inj_pfrf").unwrap();
    let store = e.injection_store();
    assert!(
        store.notice_count("planner_first_row_fetch") >= 1,
        "planner_first_row_fetch did not fire under feature-on build"
    );
    assert_eq!(
        store.notice_message("planner_first_row_fetch").as_deref(),
        Some("select_seen")
    );
}

// v7.38 P0 元机制 A — per-site testcase: index_build_post_seal.
// `CREATE INDEX <name> ON <table>(<col>)` for the BTree method
// fires this point right after `table.add_index`.
#[cfg(feature = "injection-points")]
#[test]
fn inject_index_build_post_seal_fires_on_create_index() {
    let mut e = crate::Engine::new();
    e.execute("SELECT spg_injection_attach('index_build_post_seal', 'notice:index_sealed')")
        .expect("attach succeeds");
    e.execute("CREATE TABLE inj_ibps (id INT, v INT)").unwrap();
    e.execute("CREATE INDEX inj_ibps_idx ON inj_ibps (v)")
        .unwrap();
    let store = e.injection_store();
    assert!(
        store.notice_count("index_build_post_seal") >= 1,
        "index_build_post_seal did not fire under feature-on build"
    );
    assert_eq!(
        store.notice_message("index_build_post_seal").as_deref(),
        Some("index_sealed")
    );
}

// v7.38 P0 元机制 A — per-site testcase: tx_commit_walgroup_leader_switch.
// `COMMIT` after an explicit BEGIN fires the point at the head of
// `exec_commit` (before the catalog merge). The wal_group_commit_leader_chosen
// peer point fires *after* the merge — covered by the next test.
#[cfg(feature = "injection-points")]
#[test]
fn inject_tx_commit_walgroup_leader_switch_fires_on_commit() {
    let mut e = crate::Engine::new();
    e.execute(
        "SELECT spg_injection_attach('tx_commit_walgroup_leader_switch', 'notice:commit_pre')",
    )
    .expect("attach succeeds");
    e.execute("CREATE TABLE inj_cwls (id INT)").unwrap();
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO inj_cwls VALUES (1)").unwrap();
    e.execute("COMMIT").unwrap();
    let store = e.injection_store();
    assert!(
        store.notice_count("tx_commit_walgroup_leader_switch") >= 1,
        "tx_commit_walgroup_leader_switch did not fire under feature-on build"
    );
    assert_eq!(
        store
            .notice_message("tx_commit_walgroup_leader_switch")
            .as_deref(),
        Some("commit_pre")
    );
}

// v7.38 P0 元机制 A — per-site testcase: wal_group_commit_leader_chosen.
// Same `COMMIT` flow as the previous test, but fires after the tx
// state has moved off `tx_catalogs` — confirms the post-merge
// point is reached too.
#[cfg(feature = "injection-points")]
#[test]
fn inject_wal_group_commit_leader_chosen_fires_on_commit() {
    let mut e = crate::Engine::new();
    e.execute(
        "SELECT spg_injection_attach('wal_group_commit_leader_chosen', 'notice:commit_post')",
    )
    .expect("attach succeeds");
    e.execute("CREATE TABLE inj_wgclc (id INT)").unwrap();
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO inj_wgclc VALUES (1)").unwrap();
    e.execute("COMMIT").unwrap();
    let store = e.injection_store();
    assert!(
        store.notice_count("wal_group_commit_leader_chosen") >= 1,
        "wal_group_commit_leader_chosen did not fire under feature-on build"
    );
    assert_eq!(
        store
            .notice_message("wal_group_commit_leader_chosen")
            .as_deref(),
        Some("commit_post")
    );
}

// v7.38 P0 元机制 A — the off-feature build refuses the SQL
// surface so a production SPG (no feature) can't be coerced
// into a deadlock by a malicious `SELECT spg_injection_attach`.
#[cfg(not(feature = "injection-points"))]
#[test]
fn sql_injection_attach_off_feature_errors() {
    let mut e = crate::Engine::new();
    let r = e.execute("SELECT spg_injection_attach('foo', 'wait')");
    assert!(
        r.is_err(),
        "off-feature build must refuse SQL injection-attach"
    );
}

// v7.38 Epic P (panic isolation) — the centerpiece test. Drives a
// statement that panics mid-execution (via the injection framework's
// `error` action at the SELECT executor head) and asserts the full
// isolation contract:
//   (a) the panic is caught and returned as an ordinary EngineError,
//   (b) the engine is still usable afterward,
//   (c) the panicked transaction was rolled back — no partial effect,
//   (d) the engine's RwLock is NOT poisoned by the caught panic.
// Runs under the default `panic = "unwind"` test profile; under the
// release `panic = "abort"` profile this whole path is a no-op (the
// process aborts before any unwind reaches the catch).
#[cfg(feature = "injection-points")]
#[test]
fn epic_p_panic_in_query_is_caught_and_engine_survives() {
    extern crate std;
    use std::sync::RwLock;

    let mut e = crate::Engine::new();
    e.execute("CREATE TABLE inj_panic (id INT)").unwrap();
    // Committed baseline row (autocommit).
    e.execute("INSERT INTO inj_panic VALUES (1)").unwrap();
    // Open an explicit tx and stage an UNCOMMITTED write into the
    // per-tx shadow catalog. This is the write that must vanish when
    // the panicked tx is rolled back.
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO inj_panic VALUES (2)").unwrap();

    // Grab the shared store (Arc), then wrap the engine in an RwLock
    // exactly like spg-server does, to prove the caught panic leaves it
    // un-poisoned.
    let store = e.injection_store();
    let lock = RwLock::new(e);

    // Arm a panic at the SELECT executor head. The attach SELECT itself
    // completes because the point fires *before* the projection that
    // installs the action.
    lock.write()
        .unwrap()
        .execute("SELECT spg_injection_attach('planner_first_row_fetch', 'error:boom')")
        .expect("attach succeeds");

    // (a) the panic thrown mid-SELECT is caught and returned as a normal
    //     EngineError, NOT unwound past the write guard.
    {
        let mut g = lock.write().expect("engine lock not poisoned pre-panic");
        let r = g.execute("SELECT * FROM inj_panic");
        match r {
            Err(EngineError::Internal(msg)) => {
                assert!(msg.contains("aborted"), "unexpected internal msg: {msg}");
            }
            other => panic!("expected Err(Internal), got {other:?}"),
        }
    } // (d) guard drops WITHOUT an escaping unwind → lock stays un-poisoned

    // Detach via the shared store — a SELECT here would re-trigger the
    // panic, so we reach around the SQL surface.
    store.detach("planner_first_row_fetch");

    // (d) the write lock is still acquirable — a caught panic never
    //     poisoned it. `.expect` would panic on a PoisonError.
    let mut g = lock.write().expect("engine lock poisoned by caught panic");

    // (b) engine still usable + (c) tx rolled back: only the committed
    //     row 1 survives; the in-tx INSERT(2) in the discarded shadow is
    //     gone.
    match g.execute("SELECT id FROM inj_panic").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(
                rows.len(),
                1,
                "panicked tx write should have been rolled back"
            );
            assert_eq!(rows[0].values[0], Value::Int(1));
        }
        other => panic!("expected Rows, got {other:?}"),
    }

    // The transaction is gone (rolled back), not still open.
    // v7.39 (round 435) — ask the engine directly. This used to probe by
    // running COMMIT and expecting NoActiveTransaction, which stopped being
    // a signal once a bare COMMIT became the no-op both oracles answer with.
    assert!(
        !g.in_transaction(),
        "panicked tx should have been rolled back, leaving no active tx"
    );
}

// v7.38 Epic P (panic isolation) — Slice 2 centerpiece. Mirror of
// `epic_p_panic_in_query_is_caught_and_engine_survives`, but drives the
// panicking statement through the PREPARED / EXTENDED-query path
// (`execute_prepared` → `execute_prepared_with_cancel` →
// `execute_stmt_catching` → `execute_stmt_with_cancel`) — the path sqlx /
// asyncpg / most drivers actually use via pgwire `Bind`+`Execute`. Asserts
// the same isolation contract as the simple-query slice:
//   (a) the panic is caught and returned as an ordinary EngineError,
//   (b) the engine is still usable afterward,
//   (c) the panicked transaction was rolled back — no partial effect,
//   (d) the engine's RwLock is NOT poisoned by the caught panic.
// This proves Slice 2 reuses Slice 1's rollback (`discard_tx_on_panic`)
// via the shared `catch_stmt_panic` firewall — no second policy.
#[cfg(feature = "injection-points")]
#[test]
fn epic_p_panic_in_prepared_query_is_caught_and_engine_survives() {
    extern crate std;
    use std::sync::RwLock;

    let mut e = crate::Engine::new();
    e.execute("CREATE TABLE inj_panic_prep (id INT)").unwrap();
    // Committed baseline row (autocommit).
    e.execute("INSERT INTO inj_panic_prep VALUES (1)").unwrap();
    // Open an explicit tx and stage an UNCOMMITTED write into the
    // per-tx shadow catalog. This is the write that must vanish when
    // the panicked tx is rolled back.
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO inj_panic_prep VALUES (2)").unwrap();

    // Pre-parse the panicking SELECT into a prepared `Statement` so the
    // panic drives the EXTENDED-query path (execute_prepared →
    // execute_stmt_with_cancel), NOT the simple-query path. `prepare`
    // takes `&self`, so this is fine before the engine moves into the lock.
    let prepared = e.prepare("SELECT * FROM inj_panic_prep").unwrap();

    // Grab the shared store (Arc), then wrap the engine in an RwLock
    // exactly like spg-server does, to prove the caught panic leaves it
    // un-poisoned.
    let store = e.injection_store();
    let lock = RwLock::new(e);

    // Arm a panic at the SELECT executor head. The attach SELECT itself
    // completes because the point fires *before* the projection that
    // installs the action.
    lock.write()
        .unwrap()
        .execute("SELECT spg_injection_attach('planner_first_row_fetch', 'error:boom')")
        .expect("attach succeeds");

    // (a) the panic thrown mid-Execute on the PREPARED path is caught and
    //     returned as a normal EngineError, NOT unwound past the write guard.
    {
        let mut g = lock.write().expect("engine lock not poisoned pre-panic");
        // The prepared / extended-query entry does not itself establish an
        // injection scope (only the simple-query `execute_in_with_cancel`
        // does — a no-op in production where the feature is off). Enter one
        // by hand so the armed `planner_first_row_fetch` point can fire
        // during this Execute. The guard holds no borrow of `g`, so the
        // `&mut` execute_prepared call below is fine.
        let _scope = g.enter_injection_scope();
        let r = g.execute_prepared(prepared.clone(), &[]);
        match r {
            Err(EngineError::Internal(msg)) => {
                assert!(msg.contains("aborted"), "unexpected internal msg: {msg}");
            }
            other => panic!("expected Err(Internal) from prepared path, got {other:?}"),
        }
    } // (d) guard drops WITHOUT an escaping unwind → lock stays un-poisoned

    // Detach via the shared store — a SELECT here would re-trigger the
    // panic, so we reach around the SQL surface.
    store.detach("planner_first_row_fetch");

    // (d) the write lock is still acquirable — a caught panic never
    //     poisoned it. `.expect` would panic on a PoisonError.
    let mut g = lock
        .write()
        .expect("engine lock poisoned by caught panic (prepared path)");

    // (b) engine still usable + (c) tx rolled back: only the committed
    //     row 1 survives; the in-tx INSERT(2) in the discarded shadow is
    //     gone.
    match g.execute("SELECT id FROM inj_panic_prep").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(
                rows.len(),
                1,
                "prepared-path panicked tx write should have been rolled back"
            );
            assert_eq!(rows[0].values[0], Value::Int(1));
        }
        other => panic!("expected Rows, got {other:?}"),
    }

    // The transaction is gone (rolled back), not still open.
    // v7.39 (round 435) — see the sibling test: ask the engine directly now
    // that a bare COMMIT is the no-op both oracles answer with.
    assert!(
        !g.in_transaction(),
        "prepared-path panicked tx should have been rolled back, leaving no active tx"
    );
}

// v7.38 Epic P (panic isolation) — Slice 3. Mirror of the prepared-path
// test, but drives the panic through the READ-ONLY prepared-SELECT hot
// path `execute_prepared_select_no_params` → `exec_select_cancel` — the
// pgwire `Execute` path for a bound-param-free SELECT (the bulk of driver
// read traffic). Asserts the isolation contract that applies to a
// read-only path:
//   (a) the panic is caught and returned as an ordinary EngineError,
//   (b) the engine is still usable afterward,
//   (d) the engine's RwLock is NOT poisoned by the caught panic.
// (c) tx-rollback is intentionally NOT asserted here: a SELECT opens no
// COW shadow / writer version, so the firewall's `discard_tx_on_panic` is
// a no-op on this path — there is nothing to roll back. The point of the
// firewall on the read-only paths is purely catch + survive + un-poison.
#[cfg(feature = "injection-points")]
#[test]
fn epic_p_panic_in_no_params_select_is_caught_and_engine_survives() {
    extern crate std;
    use std::sync::RwLock;

    let mut e = crate::Engine::new();
    e.execute("CREATE TABLE inj_panic_np (id INT)").unwrap();
    // Committed baseline row (autocommit) — must survive the caught panic.
    e.execute("INSERT INTO inj_panic_np VALUES (1)").unwrap();

    // Pre-parse the panicking SELECT into a `SelectStatement` so we can
    // drive the read-only no-params entry directly, exactly as pgwire does
    // for a param-free `Execute`. `prepare_select_streaming` takes `&self`,
    // so this is fine before the engine moves into the lock.
    let sel = e
        .prepare_select_streaming("SELECT * FROM inj_panic_np")
        .unwrap();

    let store = e.injection_store();
    let lock = RwLock::new(e);

    // Arm a panic at the SELECT executor head.
    lock.write()
        .unwrap()
        .execute("SELECT spg_injection_attach('planner_first_row_fetch', 'error:boom')")
        .expect("attach succeeds");

    // (a) the panic thrown inside the read-only no-params SELECT path is
    //     caught and returned as a normal EngineError, NOT unwound past the
    //     write guard.
    {
        let mut g = lock.write().expect("engine lock not poisoned pre-panic");
        // The read-only entry does not itself establish an injection scope
        // (only the simple-query `execute_in_with_cancel` does — a no-op in
        // production where the feature is off). Enter one by hand so the
        // armed point can fire during this call.
        let _scope = g.enter_injection_scope();
        let r = g.execute_prepared_select_no_params(&sel, CancelToken::none());
        match r {
            Err(EngineError::Internal(msg)) => {
                assert!(msg.contains("aborted"), "unexpected internal msg: {msg}");
            }
            other => panic!("expected Err(Internal) from no-params path, got {other:?}"),
        }
    } // (d) guard drops WITHOUT an escaping unwind → lock stays un-poisoned

    store.detach("planner_first_row_fetch");

    // (d) the write lock is still acquirable — a caught panic never
    //     poisoned it. `.expect` would panic on a PoisonError.
    let mut g = lock
        .write()
        .expect("engine lock poisoned by caught panic (no-params path)");

    // (b) engine still usable: the committed baseline row is intact and
    //     the engine takes the next query.
    match g.execute("SELECT id FROM inj_panic_np").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "committed baseline row must survive");
            assert_eq!(rows[0].values[0], Value::Int(1));
        }
        other => panic!("expected Rows, got {other:?}"),
    }
}

// v7.38 Epic P (panic isolation) — Slice 3. Same contract as the no-params
// test, but drives the panic through the STREAMING read-only SELECT path
// `execute_prepared_select_streaming` → `exec_select_streaming`. Proves
// the streaming firewall covers the streaming phase, not just setup: the
// engine drives the `emit` callback synchronously (push model) inside the
// single statement-boundary `catch_unwind`, so an unwind anywhere in the
// streaming phase is caught here.
//
// This one arms `planner_first_row_fetch`, which lives in
// `exec_select_cancel_inner` — the materialising fall-back — so it fires
// during setup, before any row is emitted. It needs a shape that still
// takes that fall-back, and which shapes those are keeps shrinking.
// `SELECT *` stopped materialising at r823, when `find_column_pos`
// learned to resolve unqualified names and a plain projection could
// bind; the arithmetic projection chosen to replace it stopped at r831,
// when a joinless single-table SELECT got a streaming walk of its own.
// Each time, an injection point on the materialising path was simply
// never reached and the test read as a firewall failure with nothing
// about the firewall changed.
//
// `ORDER BY` is a different kind of choice: it is named in the shape
// gates' explicit reject list, so it materialises by construction
// rather than by not having been optimised yet.
//
// The emit-phase half — an unwind raised once rows are already flowing —
// is what `epic_p_panic_inside_emit_callback_is_caught` below covers, and
// it matters more now that ordinary projections stream.
#[cfg(feature = "injection-points")]
#[test]
fn epic_p_panic_in_streaming_select_is_caught_and_engine_survives() {
    extern crate std;
    use std::sync::RwLock;

    let mut e = crate::Engine::new();
    e.execute("CREATE TABLE inj_panic_stream (id INT)").unwrap();
    e.execute("INSERT INTO inj_panic_stream VALUES (1)")
        .unwrap();

    let sel = e
        .prepare_select_streaming("SELECT id FROM inj_panic_stream ORDER BY id")
        .unwrap();

    let store = e.injection_store();
    let lock = RwLock::new(e);

    lock.write()
        .unwrap()
        .execute("SELECT spg_injection_attach('planner_first_row_fetch', 'error:boom')")
        .expect("attach succeeds");

    // (a) the panic thrown inside the streaming SELECT path is caught and
    //     returned as a normal EngineError, NOT unwound past the write guard.
    {
        let mut g = lock.write().expect("engine lock not poisoned pre-panic");
        let _scope = g.enter_injection_scope();
        // Benign `emit` closure, exactly the shape the wire layer supplies.
        // It never runs here because the panic fires before any Header/Row
        // is emitted (the fall-back materialises via `exec_select_cancel`
        // first, which is where `planner_first_row_fetch` fires).
        let r = g.execute_prepared_select_streaming(&sel, CancelToken::none(), |_item| Ok(()));
        match r {
            Err(EngineError::Internal(msg)) => {
                assert!(msg.contains("aborted"), "unexpected internal msg: {msg}");
            }
            other => panic!("expected Err(Internal) from streaming path, got {other:?}"),
        }
    } // (d) guard drops WITHOUT an escaping unwind → lock stays un-poisoned

    store.detach("planner_first_row_fetch");

    let mut g = lock
        .write()
        .expect("engine lock poisoned by caught panic (streaming path)");

    // (b) engine still usable after the caught streaming panic.
    match g.execute("SELECT id FROM inj_panic_stream").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "committed baseline row must survive");
            assert_eq!(rows[0].values[0], Value::Int(1));
        }
        other => panic!("expected Rows, got {other:?}"),
    }
}

// v7.37 (round 823) — the other half of the streaming firewall: an unwind
// raised AFTER rows are already flowing, rather than during setup.
//
// The test above can only reach the setup phase, because the injection
// point it arms sits on the materialising path. Nothing pinned what
// happens when the streaming phase itself unwinds — and r823 moved
// ordinary projections like `SELECT *` onto that phase, so the untested
// half is now the common one.
//
// No new injection point is needed to raise it: the `emit` callback is
// supplied by the caller and driven synchronously by the engine, so a
// panic inside it unwinds through exactly the code the firewall claims to
// cover, at exactly the moment rows are being produced.
#[cfg(feature = "injection-points")]
#[test]
fn epic_p_panic_inside_emit_callback_is_caught() {
    extern crate std;
    use std::sync::RwLock;

    let mut e = crate::Engine::new();
    e.execute("CREATE TABLE inj_panic_emit (id INT)").unwrap();
    e.execute("INSERT INTO inj_panic_emit VALUES (1),(2),(3)")
        .unwrap();

    // A plain projection, which since r823 streams rather than
    // materialising — the shape this is here to cover.
    let sel = e
        .prepare_select_streaming("SELECT * FROM inj_panic_emit")
        .unwrap();
    let lock = RwLock::new(e);

    {
        let mut g = lock.write().expect("engine lock not poisoned pre-panic");
        let _scope = g.enter_injection_scope();
        let seen = core::cell::Cell::new(0usize);
        let r = g.execute_prepared_select_streaming(&sel, CancelToken::none(), |_item| {
            seen.set(seen.get() + 1);
            // Unwind once the result is genuinely in flight.
            assert!(seen.get() < 2, "emit callback panics mid-result");
            Ok(())
        });
        match r {
            Err(EngineError::Internal(msg)) => {
                assert!(msg.contains("aborted"), "unexpected internal msg: {msg}");
            }
            other => panic!("expected Err(Internal) from the emit callback, got {other:?}"),
        }
        assert!(
            seen.get() >= 1,
            "the panic must happen after emitting started, not before"
        );
    } // guard drops WITHOUT an escaping unwind → lock stays un-poisoned

    let mut g = lock
        .write()
        .expect("engine lock poisoned by caught panic (emit callback)");
    match g.execute("SELECT id FROM inj_panic_emit").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 3, "committed rows must survive");
        }
        other => panic!("expected Rows, got {other:?}"),
    }
}

// ---------------------------------------------------------------
// v7.37 (round 830) — an authenticated login identity carries privilege
// ---------------------------------------------------------------

/// Whether a session bypasses row security is `is_superuser`, and it used
/// to answer true for every session that had not issued `SET ROLE` — so a
/// client authenticated as an ordinary role read straight past every
/// policy. Measured through psql over SCRAM before the fix: a role with
/// `rolsuper = f` saw all three rows of a table whose policy admits two.
///
/// The old default was right for what it was written for and still is:
/// in open mode the server takes any startup packet as admin, so the
/// name is a label, and letting a label carry privilege would let a
/// client name itself into a role. These three cases are that
/// distinction — the wire pins in `e2e_rls_authenticated_round830` reach
/// the same decision through `SET ROLE`, which is the half a raw-protocol
/// harness can drive; this is the half that needs a checked credential.
#[test]
fn an_authenticated_ordinary_role_is_a_policy_subject() {
    let mut e = Engine::new();
    e.create_user("alice", "pw", crate::users::Role::ReadWrite, [7u8; 16])
        .expect("create alice");
    e.role_ddl_users_mut()
        .set_attributes("alice", true, true, false);

    // Unverified: the name is a label, and the session keeps the admin
    // default. This is open mode, and it must not change.
    e.set_session_user("alice");
    assert!(
        e.is_superuser(),
        "an unverified startup name must not turn a session into a policy subject"
    );

    // Verified against a stored credential: the role's own attributes
    // decide, and this one is not a superuser.
    e.set_session_authenticated();
    assert!(
        !e.is_superuser(),
        "an authenticated ordinary role is subject to policies and grants"
    );
}

#[test]
fn an_authenticated_superuser_still_bypasses() {
    let mut e = Engine::new();
    e.create_user("su", "pw", crate::users::Role::Admin, [8u8; 16])
        .expect("create su");
    e.role_ddl_users_mut().set_attributes("su", true, true, true);
    e.set_session_user("su");
    e.set_session_authenticated();
    assert!(
        e.is_superuser(),
        "authenticating as a SUPERUSER role changes nothing"
    );
}

// ---------------------------------------------------------------
// v7.37.15 Phase C.2 — transaction status oracle
// ---------------------------------------------------------------

#[test]
fn xact_status_tracks_inflight_commit_and_abort() {
    let mut e = Engine::new();
    // Unknown version → Committed (the engine no longer tracks it;
    // frozen / pruned-old versions read as committed by definition).
    assert_eq!(e.xact_status(999), XactStatus::Committed);

    // Allocate → in-flight.
    let v = e.begin_writer_version();
    assert_eq!(e.xact_status(v), XactStatus::InProgress);

    // Commit → leaves the in-flight set the normal way → Committed.
    e.commit_writer_version(v);
    assert_eq!(e.xact_status(v), XactStatus::Committed);

    // A second version that aborts is reported Aborted, and the
    // Engine-as-oracle path (XactStatusOracle::status) agrees.
    let v2 = e.begin_writer_version();
    e.abort_writer_version(v2);
    assert_eq!(e.xact_status(v2), XactStatus::Aborted);
    assert_eq!(
        XactStatusOracle::status(&e, v2),
        XactStatus::Aborted,
        "Engine-as-oracle must delegate to xact_status"
    );
}

#[test]
fn in_tx_sees_own_insert_through_gated_window_path() {
    // Phase C.3 step 1: current_snapshot now stamps the tx's writer
    // version as tx_id (set on the engine during in-tx statement
    // execution), so a GATED scan — a window function routes through
    // `scan_visible` — inside a transaction sees the tx's own
    // uncommitted insert. Before step 1, tx_id=0 made `visible` hide
    // the row (xmin=v ∈ in_progress). Exercised through the real
    // execute() path so `current_tx` is set the way runtime sets it.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("BEGIN").unwrap();
    e.execute("INSERT INTO t VALUES (1), (2)").unwrap();
    let r = e
        .execute("SELECT id, count(*) OVER () AS c FROM t ORDER BY id")
        .unwrap();
    e.execute("COMMIT").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows from the windowed SELECT");
    };
    assert_eq!(
        rows.len(),
        2,
        "transaction must see its own two inserts through the gated window path"
    );
}

#[test]
fn mvcc_inplace_defaults_on_and_toggles() {
    let mut e = Engine::new();
    // v7.37.16 — the in-place MVCC write path is ON by default (the
    // v7.37.15 gate flipped); the `mvcc-inplace-off` feature builds the
    // legacy-path regression matrix.
    assert_eq!(
        e.mvcc_inplace(),
        !cfg!(feature = "mvcc-inplace-off"),
        "in-place MVCC default must track the mvcc-inplace-off feature"
    );
    e.set_mvcc_inplace(true);
    assert!(e.mvcc_inplace());
    e.set_mvcc_inplace(false);
    assert!(!e.mvcc_inplace());
}

#[test]
fn mvcc_inplace_delete_tombstones_but_hides_row() {
    // Phase C.3 step 4a: with the in-place gate ON, DELETE tombstones
    // the row (stamps xmax, keeps it physically present) instead of
    // physically removing it. The now-gated primary scan hides the
    // tombstoned row from a fresh snapshot, so the visible result is
    // identical to a physical delete: the two survivors.
    let mut e = Engine::new();
    e.set_mvcc_inplace(true);
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
    let del = e.execute("DELETE FROM t WHERE id = 2").unwrap();
    // affected count still reports 1 even though the row is retained.
    assert!(matches!(del, QueryResult::CommandOk { affected: 1, .. }));
    let r = e.execute("SELECT id FROM t ORDER BY id").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    assert_eq!(
        rows.len(),
        2,
        "gate-on DELETE must hide the tombstoned row via the gated scan"
    );
}

#[test]
fn mvcc_inplace_update_tombstones_old_and_shows_new() {
    // Phase C.3 step 4b: with the in-place gate ON, UPDATE tombstones
    // the old row version (stamps xmax) and appends a NEW version
    // (stamps xmin) instead of an in-place replace. The now-gated
    // primary scan hides the old version and shows the new one, so a
    // fresh snapshot sees the updated value with the row count intact.
    let mut e = Engine::new();
    e.set_mvcc_inplace(true);
    e.execute("CREATE TABLE t (id INT, v INT)").unwrap();
    e.execute("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)")
        .unwrap();
    let upd = e.execute("UPDATE t SET v = 99 WHERE id = 2").unwrap();
    // affected count still reports 1 even though the old row is retained.
    assert!(matches!(upd, QueryResult::CommandOk { affected: 1, .. }));
    let r = e.execute("SELECT id, v FROM t ORDER BY id").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    assert_eq!(
        rows.len(),
        3,
        "gate-on UPDATE must keep the row count (old version hidden, new shown)"
    );
    // The visible row for id=2 carries the NEW value, not the old 20.
    let row2 = rows
        .iter()
        .find(|row| row.values.first() == Some(&Value::Int(2)))
        .expect("id=2 must be visible");
    assert_eq!(
        row2.values.get(1),
        Some(&Value::Int(99)),
        "gate-on UPDATE must expose the new value via the gated scan"
    );
    // The old value 20 must not be visible anywhere.
    assert!(
        !rows
            .iter()
            .any(|row| row.values.get(1) == Some(&Value::Int(20))),
        "the tombstoned old version (v=20) must be hidden"
    );
}

#[test]
fn mvcc_inplace_aggregate_hides_tombstoned_row() {
    // Phase C.3 step 2: with the in-place gate ON, DELETE tombstones a
    // row instead of physically removing it. The single-table aggregate
    // full-scan path (`run_single_table_aggregate`) was ungated before
    // this step, so COUNT/SUM tallied the tombstoned row. Now gated, the
    // aggregate must exclude it: count(*)=2 and sum(id) omits id=2.
    let read_int = |v: &Value| -> i64 {
        match v {
            Value::Int(n) => i64::from(*n),
            Value::BigInt(n) => *n,
            other => panic!("expected integer aggregate, got {other:?}"),
        }
    };
    let agg_scalar = |e: &Engine, sql: &str| -> i64 {
        let QueryResult::Rows { rows, .. } = e.execute_readonly(sql).unwrap() else {
            panic!("expected Rows from {sql}");
        };
        read_int(&rows[0].values[0])
    };

    // Gate-ON: the tombstoned row must be excluded from the aggregate.
    let mut e = Engine::new();
    e.set_mvcc_inplace(true);
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
    e.execute("DELETE FROM t WHERE id = 2").unwrap();
    assert_eq!(
        agg_scalar(&e, "SELECT count(*) FROM t"),
        2,
        "gate-on aggregate must not count the tombstoned row"
    );
    assert_eq!(
        agg_scalar(&e, "SELECT sum(id) FROM t"),
        4,
        "gate-on sum(id) must exclude the tombstoned id=2 (1+3=4)"
    );

    // Gate-OFF control: physical delete, count/sum equally correct.
    let mut c = Engine::new();
    c.execute("CREATE TABLE t (id INT)").unwrap();
    c.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
    c.execute("DELETE FROM t WHERE id = 2").unwrap();
    assert_eq!(agg_scalar(&c, "SELECT count(*) FROM t"), 2);
    assert_eq!(agg_scalar(&c, "SELECT sum(id) FROM t"), 4);
}

#[test]
fn mvcc_inplace_reinsert_of_tombstoned_key_succeeds() {
    // Phase C.3: with the in-place gate ON, DELETE tombstones the row
    // (xmax stamped, row kept physically present). The unique/PK scan in
    // `constraints.rs` folds existing keys into a `seen` set by walking
    // physical rows — UNGATED before this fix, so it saw the tombstoned
    // id=2 and wrongly raised a PRIMARY KEY / UNIQUE violation on a
    // re-INSERT of the freed key. The fix skips tombstoned headers, so
    // re-inserting id=2 succeeds and exactly one id=2 is visible.
    let visible_id2_count = |e: &Engine| -> usize {
        let QueryResult::Rows { rows, .. } =
            e.execute_readonly("SELECT id FROM t WHERE id = 2").unwrap()
        else {
            panic!("expected Rows");
        };
        rows.len()
    };

    // --- PRIMARY KEY, gate ON ---
    let mut e = Engine::new();
    e.set_mvcc_inplace(true);
    e.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    e.execute("INSERT INTO t VALUES (2)").unwrap();
    e.execute("DELETE FROM t WHERE id = 2").unwrap();
    // The tombstoned key must NOT block the re-insert.
    e.execute("INSERT INTO t VALUES (2)")
        .expect("re-insert of a tombstoned PK must succeed under gate-on");
    assert_eq!(
        visible_id2_count(&e),
        1,
        "exactly one live id=2 after tombstone + re-insert (PK, gate-on)"
    );

    // --- UNIQUE, gate ON ---
    let mut u = Engine::new();
    u.set_mvcc_inplace(true);
    u.execute("CREATE TABLE t (id INT UNIQUE)").unwrap();
    u.execute("INSERT INTO t VALUES (2)").unwrap();
    u.execute("DELETE FROM t WHERE id = 2").unwrap();
    u.execute("INSERT INTO t VALUES (2)")
        .expect("re-insert of a tombstoned UNIQUE key must succeed under gate-on");
    assert_eq!(
        visible_id2_count(&u),
        1,
        "exactly one live id=2 after tombstone + re-insert (UNIQUE, gate-on)"
    );

    // --- Gate-OFF control: physical delete frees the key too ---
    let mut c = Engine::new();
    c.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    c.execute("INSERT INTO t VALUES (2)").unwrap();
    c.execute("DELETE FROM t WHERE id = 2").unwrap();
    c.execute("INSERT INTO t VALUES (2)")
        .expect("re-insert after a physical delete must succeed (gate-off)");
    assert_eq!(
        visible_id2_count(&c),
        1,
        "exactly one id=2 after physical delete + re-insert (gate-off)"
    );

    // --- Gate-ON negative control: a live duplicate still violates ---
    let mut d = Engine::new();
    d.set_mvcc_inplace(true);
    d.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    d.execute("INSERT INTO t VALUES (2)").unwrap();
    assert!(
        d.execute("INSERT INTO t VALUES (2)").is_err(),
        "a live (non-tombstoned) duplicate key must still raise a violation under gate-on"
    );
}

#[test]
fn mvcc_inplace_fk_parent_tombstone_fails_child_insert() {
    // Phase C.3: the single-column FK existence check rides the parent's
    // BTree index (`idx.lookup_eq`), which does NOT consult row headers.
    // With the in-place gate ON, DELETE tombstones the parent (xmax
    // stamped, row + index entry kept), so — before this fix — a child
    // INSERT referencing the tombstoned parent wrongly succeeded. The
    // fix skips tombstoned index locators, so the parent reads as gone
    // and the FK insert must FAIL "no parent row" (PG agrees: a deleted
    // parent violates the FK).
    let setup = |e: &mut Engine| {
        e.execute("CREATE TABLE parent (id INT PRIMARY KEY)")
            .unwrap();
        e.execute("CREATE TABLE child (id INT PRIMARY KEY, pid INT REFERENCES parent(id))")
            .unwrap();
        e.execute("INSERT INTO parent VALUES (1)").unwrap();
        e.execute("DELETE FROM parent WHERE id = 1").unwrap();
    };

    // --- Gate ON: tombstoned parent is gone → child insert fails ---
    let mut e = Engine::new();
    e.set_mvcc_inplace(true);
    setup(&mut e);
    assert!(
        e.execute("INSERT INTO child VALUES (10, 1)").is_err(),
        "child referencing a tombstoned parent must FAIL 'no parent row' (gate-on)"
    );
    // Sanity: a child referencing a still-live parent succeeds.
    e.execute("INSERT INTO parent VALUES (2)").unwrap();
    e.execute("INSERT INTO child VALUES (11, 2)")
        .expect("child referencing a live parent must succeed (gate-on)");

    // --- Gate-OFF control: physical delete removes the parent too ---
    let mut c = Engine::new();
    setup(&mut c);
    assert!(
        c.execute("INSERT INTO child VALUES (10, 1)").is_err(),
        "child referencing a physically-deleted parent must FAIL 'no parent row' (gate-off)"
    );
}

#[test]
fn mvcc_inplace_on_conflict_tombstoned_row_inserts_not_updates() {
    // Phase C.3: ON CONFLICT existence rides `on_conflict_keys_exist`
    // (single-column BTree `lookup_eq`) + `lookup_row_position_by_keys`.
    // With the in-place gate ON, DELETE tombstones id=2. Before this fix
    // the existence check saw the tombstone as a live conflict, so
    // `INSERT ... ON CONFLICT DO UPDATE` resurrected the dead row. The
    // fix treats a tombstoned hit as no-conflict, so the statement takes
    // the INSERT arm and produces exactly one live id=2 with the INSERT's
    // value — never the tombstone's.
    let visible_v = |e: &Engine| -> Vec<i32> {
        let QueryResult::Rows { rows, .. } = e
            .execute_readonly("SELECT v FROM t WHERE id = 2 ORDER BY v")
            .unwrap()
        else {
            panic!("expected Rows");
        };
        rows.iter()
            .map(|r| match r.values[0] {
                Value::Int(n) => n,
                ref other => panic!("expected Int, got {other:?}"),
            })
            .collect()
    };

    // --- Gate ON: tombstoned row is not a conflict → INSERT arm ---
    let mut e = Engine::new();
    e.set_mvcc_inplace(true);
    e.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (2, 100)").unwrap();
    e.execute("DELETE FROM t WHERE id = 2").unwrap();
    e.execute("INSERT INTO t VALUES (2, 200) ON CONFLICT (id) DO UPDATE SET v = 999")
        .expect("ON CONFLICT over a tombstoned key must take the INSERT arm (gate-on)");
    assert_eq!(
        visible_v(&e),
        vec![200],
        "tombstone + ON CONFLICT DO UPDATE inserts the new row (v=200), \
         never resurrects the tombstone as v=999 (gate-on)"
    );

    // --- Gate-ON positive control: a LIVE conflict still DO UPDATEs ---
    let mut u = Engine::new();
    u.set_mvcc_inplace(true);
    u.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)")
        .unwrap();
    u.execute("INSERT INTO t VALUES (2, 100)").unwrap();
    u.execute("INSERT INTO t VALUES (2, 200) ON CONFLICT (id) DO UPDATE SET v = 999")
        .expect("ON CONFLICT over a live key must take the UPDATE arm (gate-on)");
    assert_eq!(
        visible_v(&u),
        vec![999],
        "a live conflict still DO UPDATEs to v=999 (gate-on)"
    );

    // --- Gate-OFF control: physical delete → INSERT arm too ---
    let mut c = Engine::new();
    c.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)")
        .unwrap();
    c.execute("INSERT INTO t VALUES (2, 100)").unwrap();
    c.execute("DELETE FROM t WHERE id = 2").unwrap();
    c.execute("INSERT INTO t VALUES (2, 200) ON CONFLICT (id) DO UPDATE SET v = 999")
        .expect("ON CONFLICT after a physical delete must take the INSERT arm (gate-off)");
    assert_eq!(
        visible_v(&c),
        vec![200],
        "physical delete + ON CONFLICT DO UPDATE inserts the new row (gate-off)"
    );
}

#[test]
fn engine_row_locks_acquire_and_release() {
    use crate::locks::{LockMode, LockOutcome, WaitPolicy};
    use spg_storage::row_header::{RelId, RowId};
    let mut e = Engine::new();
    let rel = RelId(1);
    let row = RowId(7);
    // tx 100 takes an Exclusive lock.
    assert_eq!(
        e.acquire_row_lock(rel, row, LockMode::Exclusive, 100, WaitPolicy::Wait),
        LockOutcome::Granted
    );
    assert_eq!(e.locked_row_count(), 1);
    // tx 200 conflicts → blocks (or, under NoWait, unavailable).
    assert_eq!(
        e.acquire_row_lock(rel, row, LockMode::Exclusive, 200, WaitPolicy::NoWait),
        LockOutcome::NotAvailable
    );
    // Releasing tx 100's locks frees the row for tx 200.
    e.release_tx_locks(100);
    assert_eq!(
        e.acquire_row_lock(rel, row, LockMode::Exclusive, 200, WaitPolicy::Wait),
        LockOutcome::Granted
    );
    e.release_tx_locks(200);
    assert_eq!(e.locked_row_count(), 0);
}

#[test]
fn rollback_marks_writer_version_aborted() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("BEGIN").unwrap();
    // Snapshot the in-flight writer version the tx allocated, then
    // roll back and confirm the engine records it as Aborted (not
    // silently committed as the old shortcut did).
    let active_before: alloc::vec::Vec<u64> = e.active_writer_versions.iter().copied().collect();
    e.execute("ROLLBACK").unwrap();
    for v in active_before {
        assert_eq!(
            e.xact_status(v),
            XactStatus::Aborted,
            "the rolled-back tx's writer version must read Aborted"
        );
    }
}

// ---------------------------------------------------------------------
// v7.37.15 (Phase D) — engine-level vacuum: physically reclaim
// committed-tombstoned rows under gate-on, provable no-op under
// gate-off, RowId-stable across the compaction.
// ---------------------------------------------------------------------

/// Extract the rows of a `SELECT` result as `Vec<Vec<Value>>`.
fn select_values(e: &mut Engine, sql: &str) -> alloc::vec::Vec<alloc::vec::Vec<Value<'static>>> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values.clone()).collect(),
        QueryResult::CommandOk { .. } => panic!("expected Rows from `{sql}`"),
    }
}

/// Gate-on: INSERT 3, DELETE 1 (tombstone stays physically present),
/// then `vacuum_pass` reclaims the committed tombstone. Survivors keep
/// their stable RowIds + values and are still visible via SELECT.
#[test]
fn v7_37_15_phase_d_engine_vacuum_reclaims_committed_tombstone_gate_on() {
    let mut e = Engine::new();
    e.set_mvcc_inplace(true);
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
    e.execute("INSERT INTO t VALUES (2, 'b')").unwrap();
    e.execute("INSERT INTO t VALUES (3, 'c')").unwrap();
    // DELETE tombstones the row but keeps it physically present.
    e.execute("DELETE FROM t WHERE id = 2").unwrap();
    assert_eq!(
        e.catalog().get("t").unwrap().row_count(),
        3,
        "gate-on DELETE tombstones — row stays physically present"
    );
    // The deleted row is already invisible to SELECT (visibility gate).
    let before = select_values(&mut e, "SELECT id FROM t ORDER BY id");
    assert_eq!(
        before.len(),
        2,
        "SELECT hides the tombstoned row pre-vacuum"
    );

    // Capture the survivors' stable RowIds before the compaction.
    let survivors_before: alloc::vec::Vec<spg_storage::row_header::RowId> = {
        let t = e.catalog().get("t").unwrap();
        (0..t.row_count())
            .filter(|&i| {
                // survivors = rows still alive (xmax == XMAX_ALIVE)
                t.headers()
                    .get(i)
                    .map(|h| h.xmax == spg_storage::row_header::XMAX_ALIVE)
                    .unwrap_or(false)
            })
            .filter_map(|i| t.rowids().get(i).copied())
            .collect()
    };
    assert_eq!(survivors_before.len(), 2);

    // Vacuum. After the DELETE committed (autocommit), no writer is in
    // flight, so oldest_active == current_version() > the delete's xmax
    // → the tombstone is reclaimable.
    let report = e.vacuum_pass(false);
    assert_eq!(
        report.rows_reclaimed, 1,
        "the committed tombstone is reclaimed"
    );

    let t = e.catalog().get("t").unwrap();
    assert_eq!(t.row_count(), 2, "dead row is physically gone after vacuum");
    // RowId stability: the two survivors keep their exact RowIds.
    let survivors_after: alloc::vec::Vec<spg_storage::row_header::RowId> =
        t.rowids().iter().copied().collect();
    assert_eq!(
        survivors_after, survivors_before,
        "survivors keep their stable RowIds across the vacuum compaction"
    );

    // Values + visibility preserved: SELECT still returns rows 1 and 3.
    let after = select_values(&mut e, "SELECT id, name FROM t ORDER BY id");
    assert_eq!(after.len(), 2);
    assert_eq!(after[0][0], Value::Int(1));
    assert_eq!(after[0][1], Value::text("a"));
    assert_eq!(after[1][0], Value::Int(3));
    assert_eq!(after[1][1], Value::text("c"));
}

/// Gate-on conservative bound: a tombstone whose `xmax >= oldest_active`
/// (a reader could still see it) is NOT reclaimed. Holding an in-flight
/// writer version drags `oldest_active` below the delete's version;
/// once that version commits, the floor rises and the row IS reclaimed.
#[test]
fn v7_37_15_phase_d_engine_vacuum_spares_still_visible_tombstone() {
    let mut e = Engine::new();
    e.set_mvcc_inplace(true);
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    e.execute("INSERT INTO t VALUES (2)").unwrap();
    e.execute("INSERT INTO t VALUES (3)").unwrap();

    // Pin a low floor: an in-flight writer version W held open. Any
    // later delete is stamped at a version > W, so `oldest_active`
    // (== min == W) leaves it unreclaimable.
    let w = e.begin_writer_version();
    e.execute("DELETE FROM t WHERE id = 2").unwrap();
    assert!(
        e.vacuum_oldest_active() <= w,
        "an in-flight writer drags oldest_active down to its version"
    );

    let dry = e.vacuum_pass(true);
    assert_eq!(
        dry.rows_reclaimed, 0,
        "a tombstone a live reader could still see is NOT reclaimed"
    );
    assert_eq!(
        e.catalog().get("t").unwrap().row_count(),
        3,
        "no-op vacuum leaves the tombstone physically present"
    );

    // Commit the held version → floor rises to current_version(), now
    // strictly above the delete's xmax → the row becomes reclaimable.
    e.commit_writer_version(w);
    let real = e.vacuum_pass(false);
    assert_eq!(
        real.rows_reclaimed, 1,
        "reclaimable once the floor advances"
    );
    assert_eq!(e.catalog().get("t").unwrap().row_count(), 2);
}

/// Gate-off control: physical delete leaves no tombstone, so
/// `vacuum_pass` is a provable no-op and the results are unchanged.
#[test]
fn v7_37_15_phase_d_engine_vacuum_is_noop_gate_off() {
    let mut e = Engine::new();
    // This test pins the GATE-OFF contract; under the mvcc-inplace-on
    // verification feature the default is ON, so force it off.
    e.set_mvcc_inplace(false);
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    e.execute("INSERT INTO t VALUES (2)").unwrap();
    e.execute("INSERT INTO t VALUES (3)").unwrap();
    // gate-off DELETE removes the row physically — no tombstone.
    e.execute("DELETE FROM t WHERE id = 2").unwrap();
    assert_eq!(
        e.catalog().get("t").unwrap().row_count(),
        2,
        "gate-off DELETE physically removes the row"
    );

    let report = e.vacuum_pass(false);
    assert_eq!(
        report.rows_reclaimed, 0,
        "gate-off vacuum finds nothing to reclaim"
    );
    assert_eq!(
        report.rows_examined, 0,
        "gate-off vacuum does not walk tables"
    );
    assert_eq!(
        e.catalog().get("t").unwrap().row_count(),
        2,
        "gate-off vacuum leaves the table byte-for-byte unchanged"
    );

    let rows = select_values(&mut e, "SELECT id FROM t ORDER BY id");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Int(1));
    assert_eq!(rows[1][0], Value::Int(3));
}

// ── v7.37.16 autovacuum-lite ────────────────────────────────────────

#[test]
fn autovacuum_reclaims_after_threshold() {
    let mut e = Engine::new();
    e.set_mvcc_inplace(true);
    e.execute("CREATE TABLE av (id INT NOT NULL, g INT NOT NULL)")
        .unwrap();
    for i in 0..5000 {
        e.execute(&alloc::format!("INSERT INTO av VALUES ({i}, {})", i % 5))
            .unwrap();
    }
    // 4000 dead (>= 1000 absolute floor, 4000*4 >= 1000 live) — the
    // statement-exit trigger must vacuum synchronously.
    e.execute("DELETE FROM av WHERE g != 0").unwrap();
    let t = e.catalog().get("av").unwrap();
    assert_eq!(
        t.row_count(),
        1000,
        "autovacuum reclaims committed tombstones at statement exit"
    );
    assert_eq!(t.dead_rows(), 0, "meter re-based after compaction");
}

#[test]
fn autovacuum_stays_quiet_below_threshold() {
    let mut e = Engine::new();
    e.set_mvcc_inplace(true);
    e.execute("CREATE TABLE av (id INT NOT NULL)").unwrap();
    for i in 0..2000 {
        e.execute(&alloc::format!("INSERT INTO av VALUES ({i})"))
            .unwrap();
    }
    // 500 dead < 1000 absolute floor — rows stay physically present.
    e.execute("DELETE FROM av WHERE id < 500").unwrap();
    let t = e.catalog().get("av").unwrap();
    assert_eq!(t.row_count(), 2000, "below threshold — no vacuum");
    assert_eq!(t.dead_rows(), 500);
}

#[test]
fn autovacuum_defers_inside_a_transaction() {
    let mut e = Engine::new();
    e.set_mvcc_inplace(true);
    e.execute("CREATE TABLE av (id INT NOT NULL, g INT NOT NULL)")
        .unwrap();
    for i in 0..5000 {
        e.execute(&alloc::format!("INSERT INTO av VALUES ({i}, {})", i % 5))
            .unwrap();
    }
    e.execute("BEGIN").unwrap();
    e.execute("DELETE FROM av WHERE g != 0").unwrap();
    assert_eq!(
        e.catalog().get("av").unwrap().row_count(),
        5000,
        "no vacuum inside an open tx (its tombstones aren't committed)"
    );
    e.execute("COMMIT").unwrap();
    // The backlog is reclaimed by the NEXT autocommit DML on the table.
    e.execute("DELETE FROM av WHERE id = 0").unwrap();
    assert_eq!(
        e.catalog().get("av").unwrap().row_count(),
        999,
        "post-commit backlog reclaimed by the next DML's trigger"
    );
}

#[test]
fn autovacuum_disabled_leaves_tombstones() {
    let mut e = Engine::new();
    e.set_mvcc_inplace(true);
    e.set_autovacuum(false);
    e.execute("CREATE TABLE av (id INT NOT NULL, g INT NOT NULL)")
        .unwrap();
    for i in 0..5000 {
        e.execute(&alloc::format!("INSERT INTO av VALUES ({i}, {})", i % 5))
            .unwrap();
    }
    e.execute("DELETE FROM av WHERE g != 0").unwrap();
    let t = e.catalog().get("av").unwrap();
    assert_eq!(t.row_count(), 5000, "autovacuum off — tombstones stay");
    assert_eq!(t.dead_rows(), 4000);
}

/// r796 — auto-analyze counts a statement's modified rows based on
/// whether THIS connection is inside a transaction, not on whether the
/// engine has one open somewhere.
///
/// `record_modifications` was gated on the engine-wide
/// `in_transaction()`, true while any slot holds a transaction. On the
/// server every connection shares one engine, so a second connection
/// idling inside a BEGIN stopped every autocommit write from counting —
/// the table then never crosses the analyze threshold, its statistics
/// stay as they were, and the planner keeps choosing from them.
#[test]
fn another_slots_transaction_does_not_stop_autoanalyze_counting() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .unwrap();

    // A different connection's slot, left open for the rest of the test.
    let holder = crate::TxId(4242);
    e.execute_in("BEGIN", holder).unwrap();

    // 200 rows on the autocommit slot: well past the threshold, which is
    // 50 + row_count/10.
    for g in 0..200 {
        e.execute_in(
            &alloc::format!("INSERT INTO t VALUES ({g}, 'x')"),
            crate::IMPLICIT_TX,
        )
        .unwrap();
    }

    assert!(
        e.tables_needing_analyze().iter().any(|n| n == "t"),
        "200 autocommit inserts are 200 modified rows whoever else has a \
         transaction open"
    );

    e.execute_in("COMMIT", holder).unwrap();
}

