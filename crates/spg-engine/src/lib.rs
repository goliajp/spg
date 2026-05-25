//! SPG execution engine — v0.3 wires the SQL front-end to the in-memory
//! storage layer. Implements `CREATE TABLE`, single-row `INSERT VALUES`, and
//! `SELECT * FROM <table>` (no WHERE yet — that lands in v0.4 alongside
//! expression evaluation against rows).
#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use spg_sql::ast::{
    ColumnDef, ColumnTypeName, CreateTableStatement, Expr, InsertStatement, Literal,
    SelectStatement, Statement, UnOp,
};
use spg_sql::parser::{self, ParseError};
use spg_storage::{Catalog, ColumnSchema, DataType, Row, StorageError, TableSchema, Value};

/// Result of executing one statement.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryResult {
    /// DDL or DML succeeded; `affected` is the row count for `INSERT` and 0
    /// for `CREATE TABLE`.
    CommandOk { affected: usize },
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
    /// Front-end accepted a construct that the v0.3 executor doesn't support.
    Unsupported(String),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "parse: {e}"),
            Self::Storage(e) => write!(f, "storage: {e}"),
            Self::Unsupported(s) => write!(f, "unsupported: {s}"),
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

/// The execution engine. Holds the catalog and (later) other server-scope
/// state. `Engine::new()` is intentionally cheap so callers can construct one
/// per database, per test.
#[derive(Debug, Default)]
pub struct Engine {
    catalog: Catalog,
}

impl Engine {
    pub const fn new() -> Self {
        Self {
            catalog: Catalog::new(),
        }
    }

    pub const fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn execute(&mut self, sql: &str) -> Result<QueryResult, EngineError> {
        let stmt = parser::parse_statement(sql)?;
        match stmt {
            Statement::CreateTable(s) => self.exec_create_table(s),
            Statement::Insert(s) => self.exec_insert(s),
            Statement::Select(s) => self.exec_select(&s),
        }
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
        self.catalog
            .create_table(TableSchema::new(stmt.name, cols))?;
        Ok(QueryResult::CommandOk { affected: 0 })
    }

    fn exec_insert(&mut self, stmt: InsertStatement) -> Result<QueryResult, EngineError> {
        let table = self.catalog.get_mut(&stmt.table).ok_or_else(|| {
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
        Ok(QueryResult::CommandOk { affected: 1 })
    }

    fn exec_select(&self, stmt: &SelectStatement) -> Result<QueryResult, EngineError> {
        // v0.3 only supports `SELECT * FROM <table>` — no WHERE, no projection.
        if stmt.where_.is_some() {
            return Err(EngineError::Unsupported(
                "WHERE clause not supported until v0.4".into(),
            ));
        }
        let Some(from) = &stmt.from else {
            return Err(EngineError::Unsupported(
                "SELECT without FROM not supported in v0.3".into(),
            ));
        };
        if from.alias.is_some() {
            return Err(EngineError::Unsupported(
                "table aliases not supported until v0.4".into(),
            ));
        }
        if !is_wildcard_only(stmt) {
            return Err(EngineError::Unsupported(
                "only `SELECT *` is supported in v0.3".into(),
            ));
        }
        let table = self
            .catalog
            .get(&from.name)
            .ok_or_else(|| StorageError::TableNotFound {
                name: from.name.clone(),
            })?;
        Ok(QueryResult::Rows {
            columns: table.schema().columns.clone(),
            rows: table.rows().to_vec(),
        })
    }
}

fn is_wildcard_only(stmt: &SelectStatement) -> bool {
    use spg_sql::ast::SelectItem;
    stmt.items.len() == 1 && matches!(stmt.items[0], SelectItem::Wildcard)
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
            QueryResult::CommandOk { affected } => *affected,
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

    #[test]
    fn select_with_where_marked_unsupported() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE foo (a INT)").unwrap();
        let err = e.execute("SELECT * FROM foo WHERE a = 1").unwrap_err();
        assert!(matches!(err, EngineError::Unsupported(_)));
    }

    #[test]
    fn select_non_wildcard_unsupported() {
        let mut e = Engine::new();
        e.execute("CREATE TABLE foo (a INT)").unwrap();
        let err = e.execute("SELECT a FROM foo").unwrap_err();
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
}
