//! v7.16.0 — `sqlx::Row` for SPG result rows. Stores the
//! engine-side `Vec<Value>` + the column metadata so `try_get` /
//! `try_get_raw` can drive both index- and name-based lookup.

use std::sync::Arc;

use sqlx_core::HashMap;
use sqlx_core::column::ColumnIndex;
use sqlx_core::database::Database;
use sqlx_core::error::Error;
use sqlx_core::row::Row;

use spg_embedded::ValueOwned as EngineValue;

use crate::column::SpgColumn;
use crate::database::Spg;
use crate::value::SpgValueRef;

/// A single result row from an SPG-shape SELECT.
#[derive(Debug, Clone)]
pub struct SpgRow {
    /// Column metadata, shared across every row in the same
    /// fetch — `Arc` so 1-row and 1000-row result sets pay the
    /// same per-row cost.
    columns: Arc<Vec<SpgColumn>>,
    /// Name → ordinal lookup, also shared.
    by_name: Arc<HashMap<String, usize>>,
    /// The cell values.
    values: Vec<EngineValue>,
}

impl SpgRow {
    /// Construct a row given the shared column metadata + the
    /// cell values for this specific row. Adapter-internal.
    #[must_use]
    pub fn new(
        columns: Arc<Vec<SpgColumn>>,
        by_name: Arc<HashMap<String, usize>>,
        values: Vec<EngineValue>,
    ) -> Self {
        Self {
            columns,
            by_name,
            values,
        }
    }
}

impl Row for SpgRow {
    type Database = Spg;

    fn columns(&self) -> &[SpgColumn] {
        &self.columns
    }

    fn try_get_raw<I>(&self, index: I) -> Result<SpgValueRef<'_>, Error>
    where
        I: ColumnIndex<Self>,
    {
        use sqlx_core::column::Column as _;
        let ord = index.index(self)?;
        let col = self
            .columns
            .get(ord)
            .ok_or_else(|| Error::ColumnIndexOutOfBounds {
                index: ord,
                len: self.columns.len(),
            })?;
        let val = self
            .values
            .get(ord)
            .ok_or_else(|| Error::ColumnIndexOutOfBounds {
                index: ord,
                len: self.values.len(),
            })?;
        Ok(SpgValueRef::new(val, col.type_info().clone()))
    }
}

impl ColumnIndex<SpgRow> for &str {
    fn index(&self, row: &SpgRow) -> Result<usize, Error> {
        row.by_name
            .get(*self)
            .copied()
            .ok_or_else(|| Error::ColumnNotFound((*self).to_string()))
    }
}

impl ColumnIndex<SpgRow> for usize {
    fn index(&self, row: &SpgRow) -> Result<usize, Error> {
        if *self >= row.columns.len() {
            return Err(Error::ColumnIndexOutOfBounds {
                index: *self,
                len: row.columns.len(),
            });
        }
        Ok(*self)
    }
}

// `Column::type_info` returns `&<Self::Database as Database>::TypeInfo`
// which is the path Row::try_get_raw above relies on; pull it in
// at the call site via the sqlx-core trait we already imported
// to keep clippy happy when downstream consumers turn on the
// `unused_imports` lint.
const _: fn() = || {
    fn _ensure_column_trait<C: sqlx_core::column::Column>(c: &C)
    where
        C::Database: Database,
    {
        let _ = c.type_info();
    }
};
