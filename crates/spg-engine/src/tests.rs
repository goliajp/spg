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
    let positions = try_index_seek_positions(w, &schema_cols, table, "b");
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
    // `Value::Vector(Vec<f32>)` literal into the column's
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
    // converts the incoming `Value::Vector(Vec<f32>)` cell
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
        e.execute_prepared(stmt.clone(), &[Value::Int(id), Value::Text(name.into())])
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
    assert_eq!(rows[0].values[0], Value::Bytes(b"hello".to_vec()));
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
        Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef])
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
    let mut e = Engine::new();
    e.execute("CREATE TABLE foo (a INT, b TEXT)").unwrap();
    let err = e.execute("INSERT INTO foo VALUES (1)").unwrap_err();
    assert!(matches!(
        err,
        EngineError::Storage(StorageError::ArityMismatch { .. })
    ));
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
    assert_eq!(
        rows[1].values,
        vec![Value::Int(2), Value::Text("two".into())]
    );
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

fn unwrap_rows(r: QueryResult) -> (Vec<ColumnSchema>, Vec<Row>) {
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
    assert_eq!(rows[0].values[1], Value::Text("alice".into()));
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
    assert_eq!(
        rows[0].values,
        vec![Value::Text("alice".into()), Value::Int(90)]
    );
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
    assert_eq!(rows[0].values[0], Value::Text("alice".into()));
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

#[test]
fn double_begin_errors() {
    let mut e = Engine::new();
    e.execute("BEGIN").unwrap();
    let err = e.execute("BEGIN").unwrap_err();
    assert_eq!(err, EngineError::TransactionAlreadyOpen);
}

#[test]
fn commit_without_begin_errors() {
    let mut e = Engine::new();
    let err = e.execute("COMMIT").unwrap_err();
    assert_eq!(err, EngineError::NoActiveTransaction);
}

#[test]
fn rollback_without_begin_errors() {
    let mut e = Engine::new();
    let err = e.execute("ROLLBACK").unwrap_err();
    assert_eq!(err, EngineError::NoActiveTransaction);
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
fn drop_publication_silent_when_absent() {
    let mut e = Engine::new();
    // PG-compatible: DROP a publication that doesn't exist
    // succeeds (no-op) but reports zero affected.
    let r = e.execute("DROP PUBLICATION nope").unwrap();
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
                s.as_str()
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
fn drop_subscription_silent_when_absent() {
    let mut e = Engine::new();
    let r = e.execute("DROP SUBSCRIPTION never").unwrap();
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
                s.as_str()
            } else {
                panic!()
            }
        })
        .collect();
    assert_eq!(names, alloc::vec!["a_sub", "z_sub"]);
    // Row 0: a_sub
    assert_eq!(rows[0].values[1], Value::Text("h=y".to_string()));
    assert_eq!(rows[0].values[2], Value::Text("p3".to_string()));
    assert_eq!(rows[0].values[3], Value::Bool(true));
    assert_eq!(rows[0].values[4], Value::BigInt(0));
    // Row 1: z_sub — publications join with ", "
    assert_eq!(rows[1].values[2], Value::Text("p1, p2".to_string()));
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
fn auto_analyze_threshold_fires_after_10pct_of_min_rows_on_small_table() {
    // For a table with 0 rows then 10 inserts → modified=10,
    // row_count=10. Threshold = 0.1 × max(10, 100) = 10. So
    // after the 10th INSERT the threshold is met.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    for i in 0..9 {
        e.execute(&alloc::format!("INSERT INTO t VALUES ({i})"))
            .unwrap();
    }
    assert!(e.tables_needing_analyze().is_empty(), "9 < threshold");
    e.execute("INSERT INTO t VALUES (9)").unwrap();
    let needs = e.tables_needing_analyze();
    assert_eq!(needs, alloc::vec!["t".to_string()]);
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
    for i in 0..50 {
        e.execute(&alloc::format!("INSERT INTO t VALUES ({i}, 'x')"))
            .unwrap();
    }
    e.execute("ANALYZE t").unwrap();
    // UPDATE 20 rows + DELETE 5 → modified=25. Threshold = 0.1
    // × max(50, 100) = 10. So 25 >= 10 → trigger.
    e.execute("UPDATE t SET label = 'y' WHERE id < 20").unwrap();
    e.execute("DELETE FROM t WHERE id >= 45").unwrap();
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
