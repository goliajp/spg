//! SPG execution engine — v0.3 wires the SQL front-end to the in-memory
//! storage layer. Implements `CREATE TABLE`, single-row `INSERT VALUES`, and
//! `SELECT * FROM <table>` (no WHERE yet — that lands in v0.4 alongside
//! expression evaluation against rows).
#![no_std]

extern crate alloc;

pub mod eval;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use spg_sql::ast::{
    BinOp, ColumnDef, ColumnName, ColumnTypeName, CreateIndexStatement, CreateTableStatement, Expr,
    InsertStatement, Literal, SelectItem, SelectStatement, Statement, UnOp,
};
use spg_sql::parser::{self, ParseError};
use spg_storage::{
    Catalog, ColumnSchema, DataType, IndexKey, Row, StorageError, Table, TableSchema, Value,
};

use crate::eval::{EvalContext, EvalError};

/// Result of executing one statement.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryResult {
    /// DDL or DML succeeded.
    ///
    /// `affected` is the row count for `INSERT` and 0 elsewhere.
    /// `modified_catalog` tells the server whether this statement
    /// caused the *committed* catalog to change — it's the signal to
    /// snapshot/audit. False for `BEGIN`/`ROLLBACK`, false for writeful
    /// statements executed inside a transaction (those only touch the
    /// shadow), and true for `COMMIT` and for writes outside a TX.
    CommandOk {
        affected: usize,
        modified_catalog: bool,
    },
    /// `SELECT` returned a (possibly empty) row set.
    Rows {
        columns: Vec<ColumnSchema>,
        rows: Vec<Row>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum EngineError {
    Parse(ParseError),
    Storage(StorageError),
    Eval(EvalError),
    /// Front-end accepted a construct that the v0.x executor doesn't support.
    Unsupported(String),
    /// `BEGIN` while another transaction is already open.
    TransactionAlreadyOpen,
    /// `COMMIT` / `ROLLBACK` with no active transaction.
    NoActiveTransaction,
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "parse: {e}"),
            Self::Storage(e) => write!(f, "storage: {e}"),
            Self::Eval(e) => write!(f, "eval: {e}"),
            Self::Unsupported(s) => write!(f, "unsupported: {s}"),
            Self::TransactionAlreadyOpen => f.write_str("a transaction is already open"),
            Self::NoActiveTransaction => f.write_str("no active transaction"),
        }
    }
}

impl From<ParseError> for EngineError {
    fn from(e: ParseError) -> Self {
        Self::Parse(e)
    }
}
impl From<StorageError> for EngineError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}
impl From<EvalError> for EngineError {
    fn from(e: EvalError) -> Self {
        Self::Eval(e)
    }
}

/// The execution engine. Holds the catalog and (later) other server-scope
/// state. `Engine::new()` is intentionally cheap so callers can construct one
/// per database, per test.
#[derive(Debug, Default)]
pub struct Engine {
    /// Committed catalog — what survives `Engine::snapshot()` and what
    /// outside-TX `SELECT`s read.
    catalog: Catalog,
    /// While `Some(_)`, all writes go into this shadow copy. `COMMIT` swaps
    /// it into `catalog`; `ROLLBACK` drops it. SELECTs during a TX read the
    /// shadow so they see uncommitted changes (own-write visibility).
    tx_catalog: Option<Catalog>,
}

impl Engine {
    pub const fn new() -> Self {
        Self {
            catalog: Catalog::new(),
            tx_catalog: None,
        }
    }

    /// Construct an engine restored from a previously-snapshotted catalog
    /// (see `snapshot()`).
    pub const fn restore(catalog: Catalog) -> Self {
        Self {
            catalog,
            tx_catalog: None,
        }
    }

    /// The *committed* catalog. Note: during a transaction this returns the
    /// pre-TX state — `SELECT` inside a TX goes through `execute()` and reads
    /// the shadow. Tests that inspect outside-TX state should use this.
    pub const fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Serialize the *committed* catalog to bytes. v0.6 was full-snapshot; v0.9
    /// adds the rule that an open TX's shadow is never snapshotted — only the
    /// post-COMMIT state is persisted.
    pub fn snapshot(&self) -> Vec<u8> {
        self.catalog.serialize()
    }

    pub const fn in_transaction(&self) -> bool {
        self.tx_catalog.is_some()
    }

    fn active_catalog(&self) -> &Catalog {
        self.tx_catalog.as_ref().unwrap_or(&self.catalog)
    }

    fn active_catalog_mut(&mut self) -> &mut Catalog {
        if let Some(tx) = self.tx_catalog.as_mut() {
            tx
        } else {
            &mut self.catalog
        }
    }

    pub fn execute(&mut self, sql: &str) -> Result<QueryResult, EngineError> {
        let stmt = parser::parse_statement(sql)?;
        match stmt {
            Statement::CreateTable(s) => self.exec_create_table(s),
            Statement::CreateIndex(s) => self.exec_create_index(s),
            Statement::Insert(s) => self.exec_insert(s),
            Statement::Select(s) => self.exec_select(&s),
            Statement::Begin => self.exec_begin(),
            Statement::Commit => self.exec_commit(),
            Statement::Rollback => self.exec_rollback(),
        }
    }

    fn exec_begin(&mut self) -> Result<QueryResult, EngineError> {
        if self.tx_catalog.is_some() {
            return Err(EngineError::TransactionAlreadyOpen);
        }
        self.tx_catalog = Some(self.catalog.clone());
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }

    fn exec_commit(&mut self) -> Result<QueryResult, EngineError> {
        let shadow = self
            .tx_catalog
            .take()
            .ok_or(EngineError::NoActiveTransaction)?;
        self.catalog = shadow;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: true,
        })
    }

    fn exec_rollback(&mut self) -> Result<QueryResult, EngineError> {
        if self.tx_catalog.take().is_none() {
            return Err(EngineError::NoActiveTransaction);
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }

    fn exec_create_index(
        &mut self,
        stmt: CreateIndexStatement,
    ) -> Result<QueryResult, EngineError> {
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        table.add_index(stmt.name, &stmt.column)?;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: !self.in_transaction(),
        })
    }

    fn exec_create_table(
        &mut self,
        stmt: CreateTableStatement,
    ) -> Result<QueryResult, EngineError> {
        let cols = stmt
            .columns
            .into_iter()
            .map(column_def_to_schema)
            .collect::<Vec<_>>();
        self.active_catalog_mut()
            .create_table(TableSchema::new(stmt.name, cols))?;
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: !self.in_transaction(),
        })
    }

    fn exec_insert(&mut self, stmt: InsertStatement) -> Result<QueryResult, EngineError> {
        let table = self
            .active_catalog_mut()
            .get_mut(&stmt.table)
            .ok_or_else(|| {
                EngineError::Storage(StorageError::TableNotFound {
                    name: stmt.table.clone(),
                })
            })?;
        let schema = table.schema().clone();
        if stmt.values.len() != schema.columns.len() {
            return Err(EngineError::Storage(StorageError::ArityMismatch {
                expected: schema.columns.len(),
                actual: stmt.values.len(),
            }));
        }
        let mut values = Vec::with_capacity(stmt.values.len());
        for (i, (expr, col)) in stmt.values.into_iter().zip(&schema.columns).enumerate() {
            let raw = literal_expr_to_value(expr)?;
            values.push(coerce_value(raw, col.ty, &col.name, i)?);
        }
        table.insert(Row::new(values))?;
        Ok(QueryResult::CommandOk {
            affected: 1,
            modified_catalog: !self.in_transaction(),
        })
    }

    fn exec_select(&self, stmt: &SelectStatement) -> Result<QueryResult, EngineError> {
        let Some(from) = &stmt.from else {
            return Err(EngineError::Unsupported(
                "SELECT without FROM not supported yet".into(),
            ));
        };
        let table =
            self.active_catalog()
                .get(&from.name)
                .ok_or_else(|| StorageError::TableNotFound {
                    name: from.name.clone(),
                })?;
        let schema_cols = &table.schema().columns;
        // The qualifier accepted on column refs is the alias (if any) else the
        // bare table name.
        let alias = from.alias.as_deref().unwrap_or(from.name.as_str());
        let ctx = EvalContext::new(schema_cols, Some(alias));

        let projection = build_projection(&stmt.items, schema_cols, alias)?;

        // Index seek: if WHERE is `col = literal` (or commuted) and the
        // referenced column has an index, iterate only the matching row
        // indices. Otherwise fall back to a full scan.
        let candidate_rows: Vec<usize> = stmt
            .where_
            .as_ref()
            .and_then(|w| try_index_seek(w, schema_cols, table, alias))
            .unwrap_or_else(|| (0..table.row_count()).collect());

        // Materialise the filter pass into `(order_key, projected_row)`
        // tuples. The order key is `None` when there's no ORDER BY clause.
        let mut tagged: Vec<(Option<f64>, Row)> = Vec::new();
        for &i in &candidate_rows {
            let row = &table.rows()[i];
            if let Some(where_expr) = &stmt.where_ {
                let cond = eval::eval_expr(where_expr, row, &ctx)?;
                if !matches!(cond, Value::Bool(true)) {
                    continue;
                }
            }
            let mut values = Vec::with_capacity(projection.len());
            for p in &projection {
                values.push(eval::eval_expr(&p.expr, row, &ctx)?);
            }
            let order_key = if let Some(order_expr) = &stmt.order_by {
                let key = eval::eval_expr(order_expr, row, &ctx)?;
                Some(value_to_order_key(&key)?)
            } else {
                None
            };
            tagged.push((order_key, Row::new(values)));
        }

        if stmt.order_by.is_some() {
            tagged.sort_by(|a, b| {
                let ka = a.0.unwrap_or(f64::INFINITY);
                let kb = b.0.unwrap_or(f64::INFINITY);
                ka.partial_cmp(&kb).unwrap_or(core::cmp::Ordering::Equal)
            });
        }

        let mut output_rows: Vec<Row> = tagged.into_iter().map(|(_, r)| r).collect();
        if let Some(n) = stmt.limit {
            output_rows.truncate(n as usize);
        }

        let columns: Vec<ColumnSchema> = projection
            .into_iter()
            .map(|p| ColumnSchema::new(p.output_name, p.ty, p.nullable))
            .collect();

        Ok(QueryResult::Rows {
            columns,
            rows: output_rows,
        })
    }
}

/// One row-producing projection: an expression to evaluate, the resulting
/// column's user-visible name, its inferred type, and nullability.
#[derive(Debug, Clone)]
struct ProjectedItem {
    expr: Expr,
    output_name: String,
    ty: DataType,
    nullable: bool,
}

/// Coerce a `Value` to an `f64` sort key for ORDER BY. Numbers map directly;
/// NULL sorts last (treated as `+∞`); booleans are 0.0 / 1.0; text uses lex
/// order via the byte values; vectors are not sortable.
fn value_to_order_key(v: &Value) -> Result<f64, EngineError> {
    match v {
        Value::Null => Ok(f64::INFINITY),
        Value::Int(n) => Ok(f64::from(*n)),
        #[allow(clippy::cast_precision_loss)]
        Value::BigInt(n) => Ok(*n as f64),
        Value::Float(x) => Ok(*x),
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        Value::Text(s) => {
            // Lex order by codepoints — good enough for ORDER BY name.
            // Map first 8 bytes packed into u64 as a coarse key; ties fall to
            // partial_cmp Equal. v1.x can swap in a real string comparator.
            let mut key: u64 = 0;
            for &b in s.as_bytes().iter().take(8) {
                key = (key << 8) | u64::from(b);
            }
            #[allow(clippy::cast_precision_loss)]
            Ok(key as f64)
        }
        Value::Vector(_) => Err(EngineError::Unsupported(
            "ORDER BY of a raw vector column is not meaningful — use `<->`".into(),
        )),
    }
}

/// Try to plan a WHERE clause as an equality lookup against an existing
/// index. Returns the candidate row indices on success; `None` means the
/// caller should fall back to a full scan.
///
/// v0.8 recognises a single top-level `col = literal` (in either operand
/// order). AND chains and range scans land in later milestones.
fn try_index_seek(
    where_expr: &Expr,
    schema_cols: &[ColumnSchema],
    table: &Table,
    table_alias: &str,
) -> Option<Vec<usize>> {
    let Expr::Binary {
        lhs,
        op: BinOp::Eq,
        rhs,
    } = where_expr
    else {
        return None;
    };
    let (col_pos, value) = resolve_col_literal_pair(lhs, rhs, schema_cols, table_alias)
        .or_else(|| resolve_col_literal_pair(rhs, lhs, schema_cols, table_alias))?;
    let idx = table.index_on(col_pos)?;
    let key = IndexKey::from_value(&value)?;
    Some(idx.lookup_eq(&key).to_vec())
}

fn resolve_col_literal_pair(
    col_side: &Expr,
    lit_side: &Expr,
    schema_cols: &[ColumnSchema],
    table_alias: &str,
) -> Option<(usize, Value)> {
    let Expr::Column(c) = col_side else {
        return None;
    };
    if let Some(q) = &c.qualifier
        && q != table_alias
    {
        return None;
    }
    let pos = schema_cols.iter().position(|s| s.name == c.name)?;
    let Expr::Literal(l) = lit_side else {
        return None;
    };
    let v = match l {
        Literal::Integer(n) => {
            if let Ok(small) = i32::try_from(*n) {
                Value::Int(small)
            } else {
                Value::BigInt(*n)
            }
        }
        Literal::Float(x) => Value::Float(*x),
        Literal::String(s) => Value::Text(s.clone()),
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Null => Value::Null,
        // Vector literals can't be used as B-tree index keys (Vec<f32> isn't
        // Ord). Tell the planner to fall back to full-scan.
        Literal::Vector(_) => return None,
    };
    Some((pos, v))
}

fn build_projection(
    items: &[SelectItem],
    schema_cols: &[ColumnSchema],
    table_alias: &str,
) -> Result<Vec<ProjectedItem>, EngineError> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard => {
                for col in schema_cols {
                    out.push(ProjectedItem {
                        expr: Expr::Column(ColumnName {
                            qualifier: None,
                            name: col.name.clone(),
                        }),
                        output_name: col.name.clone(),
                        ty: col.ty,
                        nullable: col.nullable,
                    });
                }
            }
            SelectItem::Expr { expr, alias } => {
                let Expr::Column(c) = expr else {
                    return Err(EngineError::Unsupported(
                        "non-column SELECT expressions not supported until v0.5".into(),
                    ));
                };
                if let Some(q) = &c.qualifier
                    && q != table_alias
                {
                    return Err(EngineError::Eval(EvalError::UnknownQualifier {
                        qualifier: q.clone(),
                    }));
                }
                let sch = schema_cols
                    .iter()
                    .find(|s| s.name == c.name)
                    .ok_or_else(|| EvalError::ColumnNotFound {
                        name: c.name.clone(),
                    })?;
                let output_name = alias.clone().unwrap_or_else(|| sch.name.clone());
                out.push(ProjectedItem {
                    expr: expr.clone(),
                    output_name,
                    ty: sch.ty,
                    nullable: sch.nullable,
                });
            }
        }
    }
    Ok(out)
}

fn column_def_to_schema(c: ColumnDef) -> ColumnSchema {
    ColumnSchema::new(c.name, column_type_to_data_type(c.ty), c.nullable)
}

const fn column_type_to_data_type(t: ColumnTypeName) -> DataType {
    match t {
        ColumnTypeName::Int => DataType::Int,
        ColumnTypeName::BigInt => DataType::BigInt,
        ColumnTypeName::Float => DataType::Float,
        ColumnTypeName::Text => DataType::Text,
        ColumnTypeName::Bool => DataType::Bool,
        ColumnTypeName::Vector(n) => DataType::Vector(n),
    }
}

/// Convert an INSERT VALUES expression to a storage Value. v0.3 supports
/// literal expressions and unary-minus over numeric literals — anything more
/// complex (column refs, binary ops, etc.) returns `Unsupported`.
fn literal_expr_to_value(expr: Expr) -> Result<Value, EngineError> {
    match expr {
        Expr::Literal(l) => Ok(literal_to_value(l)),
        Expr::Unary {
            op: UnOp::Neg,
            expr,
        } => match *expr {
            Expr::Literal(Literal::Integer(n)) => {
                // Fold to i32 if it fits, else BigInt. Parser emits Integer(i64)
                // — overflow on negate of i64::MIN is the one edge case.
                let neg = n.checked_neg().ok_or_else(|| {
                    EngineError::Unsupported("integer literal overflow on negation".into())
                })?;
                Ok(int_value_for(neg))
            }
            Expr::Literal(Literal::Float(x)) => Ok(Value::Float(-x)),
            other => Err(EngineError::Unsupported(alloc::format!(
                "unary minus over non-literal expression: {other:?}"
            ))),
        },
        other => Err(EngineError::Unsupported(alloc::format!(
            "non-literal INSERT value expression: {other:?}"
        ))),
    }
}

fn literal_to_value(l: Literal) -> Value {
    match l {
        Literal::Integer(n) => int_value_for(n),
        Literal::Float(x) => Value::Float(x),
        Literal::String(s) => Value::Text(s),
        Literal::Bool(b) => Value::Bool(b),
        Literal::Null => Value::Null,
        Literal::Vector(v) => Value::Vector(v),
    }
}

/// Pick `Int` (`i32`) when the literal fits, else `BigInt`. `INT` vs `BIGINT`
/// columns will still enforce the right tag downstream — this is just the
/// default we synthesise from an unannotated integer literal.
fn int_value_for(n: i64) -> Value {
    if let Ok(small) = i32::try_from(n) {
        Value::Int(small)
    } else {
        Value::BigInt(n)
    }
}

/// Widen / narrow `v` to fit `expected`. Numerics permit safe widening
/// (`Int → BigInt`, `Int/BigInt → Float`) and best-effort narrowing
/// (`BigInt → Int` succeeds only when the value fits in `i32`). Everything
/// else returns `TypeMismatch` carrying the column name for caller diagnostics.
/// `NULL` is always permitted; the nullability check happens later in storage.
fn coerce_value(
    v: Value,
    expected: DataType,
    col_name: &str,
    position: usize,
) -> Result<Value, EngineError> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    let actual = v.data_type().expect("non-null");
    if actual == expected {
        return Ok(v);
    }
    let coerced = match (v, expected) {
        (Value::Int(n), DataType::BigInt) => Some(Value::BigInt(i64::from(n))),
        (Value::Int(n), DataType::Float) => Some(Value::Float(f64::from(n))),
        (Value::BigInt(n), DataType::Int) => i32::try_from(n).ok().map(Value::Int),
        #[allow(clippy::cast_precision_loss)]
        (Value::BigInt(n), DataType::Float) => Some(Value::Float(n as f64)),
        _ => None,
    };
    coerced.ok_or(EngineError::Storage(StorageError::TypeMismatch {
        column: col_name.into(),
        expected,
        actual,
        position,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn unwrap_command_ok(r: &QueryResult) -> usize {
        match r {
            QueryResult::CommandOk { affected, .. } => *affected,
            QueryResult::Rows { .. } => panic!("expected CommandOk, got Rows"),
        }
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
    fn insert_non_literal_expr_unsupported() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE foo (a INT NOT NULL)").unwrap();
        let err = e.execute("INSERT INTO foo VALUES (1 + 2)").unwrap_err();
        assert!(matches!(err, EngineError::Unsupported(_)));
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
    fn expression_projection_still_unsupported() {
        // `1 + 2` as a select item — out of scope until v0.5.
        let mut e = Engine::new();
        e.execute("CREATE TABLE t (a INT)").unwrap();
        let err = e.execute("SELECT 1 + 2 FROM t").unwrap_err();
        assert!(matches!(err, EngineError::Unsupported(_)));
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
        let mut e = Engine::new();
        let err = e.execute("UPDATE foo SET x = 1").unwrap_err();
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
}
