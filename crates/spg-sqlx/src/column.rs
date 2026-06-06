//! v7.16.0 — `sqlx::Column` for SPG result-set columns.

use sqlx_core::column::Column;
use sqlx_core::ext::ustr::UStr;

use crate::database::Spg;
use crate::type_info::SpgTypeInfo;

/// Per-column metadata in an SPG result set. mailrs's
/// `#[derive(FromRow)]` reads columns by name via
/// [`Row::column`][sqlx_core::row::Row::column] which calls
/// `name()` on this type.
#[derive(Debug, Clone)]
pub struct SpgColumn {
    ordinal: usize,
    name: UStr,
    type_info: SpgTypeInfo,
}

impl SpgColumn {
    /// Construct a column descriptor. Internal — typically
    /// produced by the adapter when it converts an engine
    /// `Vec<ColumnSchema>` into the sqlx `Row` shape.
    #[must_use]
    pub fn new(ordinal: usize, name: impl Into<String>, type_info: SpgTypeInfo) -> Self {
        // UStr stores a &'static str OR owns its bytes — using
        // the `new` constructor copies the input into an
        // internal Arc so the result is `'static`-borrow-shaped
        // without leaking.
        let owned: String = name.into();
        Self {
            ordinal,
            name: UStr::new(&owned),
            type_info,
        }
    }
}

impl Column for SpgColumn {
    type Database = Spg;

    fn ordinal(&self) -> usize {
        self.ordinal
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn type_info(&self) -> &SpgTypeInfo {
        &self.type_info
    }
}
