//! In-memory storage primitives.
//!
//! v0.3 is intentionally simple: a flat catalog of tables, each holding rows
//! as `Vec<Value>` (positional, matching the table's `TableSchema`). No MVCC,
//! no on-disk format — those land in later milestones.
#![no_std]

extern crate alloc;

use alloc::format;
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
    /// On-disk format failed to parse — corrupted file, wrong magic, truncated
    /// payload, or unknown tag bytes.
    Corrupt(String),
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
            Self::Corrupt(detail) => write!(f, "corrupt on-disk format: {detail}"),
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

// =========================================================================
// Persistent binary format for the catalog (v0.6).
//
// Layout (little-endian throughout):
//
//   [magic "SPGDB001" 8 bytes][version u8]
//   [table_count u32]
//   for each table:
//       [name_len u16][name bytes]
//       [col_count u16]
//       for each col:
//           [name_len u16][name bytes]
//           [type_tag u8]   1=Int 2=BigInt 3=Float 4=Text 5=Bool
//           [nullable u8]   0/1
//       [row_count u32]
//       for each row, for each col, one [value_tag u8] + value bytes:
//           tag 0 (Null)   → no body
//           tag 1 (Int)    → i32 LE
//           tag 2 (BigInt) → i64 LE
//           tag 3 (Float)  → f64 LE
//           tag 4 (Text)   → u16 LE len + UTF-8 bytes
//           tag 5 (Bool)   → u8 0/1
// =========================================================================

const FILE_MAGIC: &[u8; 8] = b"SPGDB001";
const FILE_VERSION: u8 = 1;

impl Catalog {
    /// Serialize the whole catalog (schema + every row) into a self-contained
    /// byte buffer. Format is documented above the impl block.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(FILE_MAGIC);
        out.push(FILE_VERSION);
        write_u32(
            &mut out,
            u32::try_from(self.tables.len()).expect("≤ 4G tables"),
        );
        for t in &self.tables {
            write_str(&mut out, &t.schema.name);
            write_u16(
                &mut out,
                u16::try_from(t.schema.columns.len()).expect("≤ 65k columns/table"),
            );
            for c in &t.schema.columns {
                write_str(&mut out, &c.name);
                out.push(data_type_tag(c.ty));
                out.push(u8::from(c.nullable));
            }
            write_u32(
                &mut out,
                u32::try_from(t.rows.len()).expect("≤ 4G rows/table"),
            );
            for row in &t.rows {
                for v in &row.values {
                    write_value(&mut out, v);
                }
            }
        }
        out
    }

    /// Deserialize a previously-serialized catalog. Rejects bad magic, version
    /// mismatch, unknown tags, truncation, and trailing bytes.
    pub fn deserialize(buf: &[u8]) -> Result<Self, StorageError> {
        let mut cur = Cursor::new(buf);
        let magic = cur.take(8)?;
        if magic != FILE_MAGIC {
            return Err(StorageError::Corrupt(format!(
                "bad magic: expected SPGDB001, got {magic:?}"
            )));
        }
        let version = cur.read_u8()?;
        if version != FILE_VERSION {
            return Err(StorageError::Corrupt(format!(
                "unsupported file version: {version}"
            )));
        }
        let table_count = cur.read_u32()? as usize;
        let mut cat = Self::new();
        for _ in 0..table_count {
            let name = cur.read_str()?;
            let col_count = cur.read_u16()? as usize;
            let mut cols = Vec::with_capacity(col_count);
            for _ in 0..col_count {
                let c_name = cur.read_str()?;
                let ty = data_type_from_tag(cur.read_u8()?)?;
                let nullable = cur.read_u8()? != 0;
                cols.push(ColumnSchema {
                    name: c_name,
                    ty,
                    nullable,
                });
            }
            cat.create_table(TableSchema::new(name.clone(), cols))?;
            let n_cols = cat.get(&name).expect("just inserted").schema.columns.len();
            let row_count = cur.read_u32()? as usize;
            for _ in 0..row_count {
                let mut values = Vec::with_capacity(n_cols);
                for _ in 0..n_cols {
                    values.push(cur.read_value()?);
                }
                let t = cat.get_mut(&name).expect("just inserted");
                t.rows.push(Row { values });
            }
        }
        if cur.pos < buf.len() {
            return Err(StorageError::Corrupt(format!(
                "trailing bytes: {} unread",
                buf.len() - cur.pos
            )));
        }
        Ok(cat)
    }
}

// --- low-level binary helpers ---------------------------------------------

const fn data_type_tag(t: DataType) -> u8 {
    match t {
        DataType::Int => 1,
        DataType::BigInt => 2,
        DataType::Float => 3,
        DataType::Text => 4,
        DataType::Bool => 5,
    }
}

fn data_type_from_tag(tag: u8) -> Result<DataType, StorageError> {
    match tag {
        1 => Ok(DataType::Int),
        2 => Ok(DataType::BigInt),
        3 => Ok(DataType::Float),
        4 => Ok(DataType::Text),
        5 => Ok(DataType::Bool),
        other => Err(StorageError::Corrupt(format!(
            "unknown data type tag: {other}"
        ))),
    }
}

fn write_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.push(0),
        Value::Int(n) => {
            out.push(1);
            out.extend_from_slice(&n.to_le_bytes());
        }
        Value::BigInt(n) => {
            out.push(2);
            out.extend_from_slice(&n.to_le_bytes());
        }
        Value::Float(x) => {
            out.push(3);
            out.extend_from_slice(&x.to_le_bytes());
        }
        Value::Text(s) => {
            out.push(4);
            write_str(out, s);
        }
        Value::Bool(b) => {
            out.push(5);
            out.push(u8::from(*b));
        }
    }
}

fn write_u16(out: &mut Vec<u8>, n: u16) {
    out.extend_from_slice(&n.to_le_bytes());
}
fn write_u32(out: &mut Vec<u8>, n: u32) {
    out.extend_from_slice(&n.to_le_bytes());
}
fn write_str(out: &mut Vec<u8>, s: &str) {
    let len = u16::try_from(s.len()).expect("identifier / text fits in u16");
    write_u16(out, len);
    out.extend_from_slice(s.as_bytes());
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], StorageError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| StorageError::Corrupt(format!("length overflow taking {n} bytes")))?;
        if end > self.buf.len() {
            return Err(StorageError::Corrupt(format!(
                "unexpected EOF at offset {} (wanted {n} more bytes)",
                self.pos
            )));
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn read_u8(&mut self) -> Result<u8, StorageError> {
        Ok(self.take(1)?[0])
    }
    fn read_u16(&mut self) -> Result<u16, StorageError> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }
    fn read_u32(&mut self) -> Result<u32, StorageError> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn read_i32(&mut self) -> Result<i32, StorageError> {
        let s = self.take(4)?;
        Ok(i32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn read_i64(&mut self) -> Result<i64, StorageError> {
        let s = self.take(8)?;
        let arr: [u8; 8] = s.try_into().expect("checked");
        Ok(i64::from_le_bytes(arr))
    }
    fn read_f64(&mut self) -> Result<f64, StorageError> {
        let s = self.take(8)?;
        let arr: [u8; 8] = s.try_into().expect("checked");
        Ok(f64::from_le_bytes(arr))
    }
    fn read_str(&mut self) -> Result<String, StorageError> {
        let len = self.read_u16()? as usize;
        let bytes = self.take(len)?;
        core::str::from_utf8(bytes)
            .map(String::from)
            .map_err(|_| StorageError::Corrupt("invalid UTF-8 in identifier or text".into()))
    }
    fn read_value(&mut self) -> Result<Value, StorageError> {
        let tag = self.read_u8()?;
        match tag {
            0 => Ok(Value::Null),
            1 => Ok(Value::Int(self.read_i32()?)),
            2 => Ok(Value::BigInt(self.read_i64()?)),
            3 => Ok(Value::Float(self.read_f64()?)),
            4 => Ok(Value::Text(self.read_str()?)),
            5 => Ok(Value::Bool(self.read_u8()? != 0)),
            other => Err(StorageError::Corrupt(format!("unknown value tag: {other}"))),
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

    // --- v0.6 persistence round-trips --------------------------------------

    fn assert_round_trip(cat: &Catalog) {
        let bytes = cat.serialize();
        let restored = Catalog::deserialize(&bytes).expect("deserialize");
        // Compare semantic state: same tables in same order, same schema +
        // rows in each.
        assert_eq!(restored.table_count(), cat.table_count());
        for (a, b) in cat.tables.iter().zip(&restored.tables) {
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
    fn serialize_table_with_rows_round_trips() {
        let mut cat = Catalog::new();
        cat.create_table(make_users_schema()).unwrap();
        let t = cat.get_mut("users").unwrap();
        t.insert(Row::new(vec![
            Value::Int(1),
            Value::Text("alice".into()),
            Value::Float(95.5),
        ]))
        .unwrap();
        t.insert(Row::new(vec![
            Value::Int(2),
            Value::Text("bob".into()),
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
}
