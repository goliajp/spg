//! In-memory storage primitives.
//!
//! v0.3 is intentionally simple: a flat catalog of tables, each holding rows
//! as `Vec<Value>` (positional, matching the table's `TableSchema`). No MVCC,
//! no on-disk format — those land in later milestones.
#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// Runtime type tags, matching the PG types SPG accepts in v0.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    Int,    // 32-bit signed
    BigInt, // 64-bit signed
    Float,  // f64 (PG double precision)
    Text,
    Bool,
}

impl DataType {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Int => "INT",
            Self::BigInt => "BIGINT",
            Self::Float => "FLOAT",
            Self::Text => "TEXT",
            Self::Bool => "BOOL",
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A row-cell value, including SQL `NULL`. `Float` uses `f64`; NaN compares
/// non-equal to itself (PG behaviour) — `PartialEq` is derived so callers
/// must opt into NaN-aware comparison if they need stronger guarantees.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i32),
    BigInt(i64),
    Float(f64),
    Text(String),
    Bool(bool),
    Null,
}

impl Value {
    /// Type tag, or `None` for `NULL` (unknown at value level).
    pub const fn data_type(&self) -> Option<DataType> {
        match self {
            Self::Int(_) => Some(DataType::Int),
            Self::BigInt(_) => Some(DataType::BigInt),
            Self::Float(_) => Some(DataType::Float),
            Self::Text(_) => Some(DataType::Text),
            Self::Bool(_) => Some(DataType::Bool),
            Self::Null => None,
        }
    }

    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

/// One table row — values are positional and must match
/// `TableSchema.columns` in length and (modulo NULL) in `DataType`.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub values: Vec<Value>,
}

impl Row {
    pub const fn new(values: Vec<Value>) -> Self {
        Self { values }
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSchema {
    pub name: String,
    pub ty: DataType,
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<ColumnSchema>,
}

impl TableSchema {
    pub fn column_position(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }
}

/// In-memory table: schema + a flat row vector. Row order is insertion order;
/// v0.3 makes no ordering guarantees beyond that.
#[derive(Debug, Clone)]
pub struct Table {
    schema: TableSchema,
    rows: Vec<Row>,
}

impl Table {
    pub const fn new(schema: TableSchema) -> Self {
        Self {
            schema,
            rows: Vec::new(),
        }
    }

    pub const fn schema(&self) -> &TableSchema {
        &self.schema
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Insert one row after validating it matches the schema (length + type).
    /// Returns `StorageError` on mismatch — the table is left unchanged.
    pub fn insert(&mut self, row: Row) -> Result<(), StorageError> {
        if row.len() != self.schema.columns.len() {
            return Err(StorageError::ArityMismatch {
                expected: self.schema.columns.len(),
                actual: row.len(),
            });
        }
        for (i, (val, col)) in row.values.iter().zip(&self.schema.columns).enumerate() {
            if val.is_null() {
                if !col.nullable {
                    return Err(StorageError::NullInNotNull {
                        column: col.name.clone(),
                    });
                }
                continue;
            }
            let actual = val.data_type().expect("non-null");
            if actual != col.ty {
                return Err(StorageError::TypeMismatch {
                    column: col.name.clone(),
                    expected: col.ty,
                    actual,
                    position: i,
                });
            }
        }
        self.rows.push(row);
        Ok(())
    }
}

/// Flat catalog. `Vec` is intentional — std `HashMap` is unavailable in
/// `no_std` + alloc, and v0.3 is single-database with a small table count.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    tables: Vec<Table>,
}

impl Catalog {
    pub const fn new() -> Self {
        Self { tables: Vec::new() }
    }

    pub fn create_table(&mut self, schema: TableSchema) -> Result<(), StorageError> {
        if self.tables.iter().any(|t| t.schema.name == schema.name) {
            return Err(StorageError::DuplicateTable {
                name: schema.name.clone(),
            });
        }
        self.tables.push(Table::new(schema));
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|t| t.schema.name == name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Table> {
        self.tables.iter_mut().find(|t| t.schema.name == name)
    }

    pub fn table_count(&self) -> usize {
        self.tables.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    DuplicateTable {
        name: String,
    },
    TableNotFound {
        name: String,
    },
    ArityMismatch {
        expected: usize,
        actual: usize,
    },
    TypeMismatch {
        column: String,
        expected: DataType,
        actual: DataType,
        position: usize,
    },
    NullInNotNull {
        column: String,
    },
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateTable { name } => write!(f, "table already exists: {name}"),
            Self::TableNotFound { name } => write!(f, "table not found: {name}"),
            Self::ArityMismatch { expected, actual } => write!(
                f,
                "row arity mismatch: expected {expected} columns, got {actual}"
            ),
            Self::TypeMismatch {
                column,
                expected,
                actual,
                position,
            } => write!(
                f,
                "type mismatch in column {column:?} (position {position}): expected {expected}, got {actual}"
            ),
            Self::NullInNotNull { column } => {
                write!(f, "NULL value in NOT NULL column {column:?}")
            }
        }
    }
}

impl ColumnSchema {
    pub fn new(name: impl Into<String>, ty: DataType, nullable: bool) -> Self {
        Self {
            name: name.into(),
            ty,
            nullable,
        }
    }
}

impl TableSchema {
    pub fn new(name: impl Into<String>, columns: Vec<ColumnSchema>) -> Self {
        Self {
            name: name.into(),
            columns,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

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
        assert_eq!(Value::Text("x".into()).data_type(), Some(DataType::Text));
        assert_eq!(Value::Bool(true).data_type(), Some(DataType::Bool));
        assert_eq!(Value::Null.data_type(), None);
        assert!(Value::Null.is_null());
        assert!(!Value::Int(0).is_null());
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
            Value::Text("alice".into()),
            Value::Float(99.5),
        ]))
        .unwrap();
        assert_eq!(t.row_count(), 1);
        assert_eq!(t.rows()[0].values[1], Value::Text("alice".into()));
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
            Value::Text("bob".into()),
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
}
