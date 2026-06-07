//! v7.16.0 — `sqlx::Statement` for SPG. Wraps the
//! [`spg_embedded::Statement`] handle returned by the embedded
//! `prepare` API so prepared-statement reuse flows through.

use std::borrow::Cow;
use std::sync::Arc;

use either::Either;
use sqlx_core::HashMap;
use sqlx_core::column::ColumnIndex;
use sqlx_core::error::Error;
use sqlx_core::statement::Statement;

use crate::column::SpgColumn;
use crate::database::Spg;
use crate::type_info::SpgTypeInfo;

/// Prepared-statement handle. Holds:
///   * the source SQL (for [`Statement::sql`]),
///   * the embedded `Statement` AST handle the engine cached
///     during prepare,
///   * the expected output columns (populated on first execute
///     that returns rows; empty until then),
///   * an empty type-name → column-ordinal map shared with
///     `SpgRow::ColumnIndex<&str>`.
#[derive(Debug, Clone)]
pub struct SpgStatement<'q> {
    pub(crate) sql: Cow<'q, str>,
    /// `Some` when the engine has produced a planned AST; `None`
    /// for `Statement::to_owned` rebuilds where the cached AST
    /// is not in hand.
    pub(crate) inner: Option<Arc<spg_embedded::Statement>>,
    pub(crate) columns: Arc<Vec<SpgColumn>>,
    pub(crate) by_name: Arc<HashMap<String, usize>>,
}

impl<'q> SpgStatement<'q> {
    /// Build a statement handle with no parsed-AST attached
    /// (e.g. for `sqlx::query("...").execute(&pool)` where the
    /// adapter prepares on-the-fly inside the connection).
    #[must_use]
    pub fn unparsed(sql: impl Into<Cow<'q, str>>) -> Self {
        Self {
            sql: sql.into(),
            inner: None,
            columns: Arc::new(Vec::new()),
            by_name: Arc::new(HashMap::new()),
        }
    }
}

impl<'q> Statement<'q> for SpgStatement<'q> {
    type Database = Spg;

    fn to_owned(&self) -> SpgStatement<'static> {
        SpgStatement {
            sql: Cow::Owned(self.sql.clone().into_owned()),
            inner: self.inner.clone(),
            columns: Arc::clone(&self.columns),
            by_name: Arc::clone(&self.by_name),
        }
    }

    fn sql(&self) -> &str {
        &self.sql
    }

    fn parameters(&self) -> Option<Either<&[SpgTypeInfo], usize>> {
        // v7.16.0 — SPG-engine plan doesn't surface per-
        // placeholder type info today; we hand sqlx the count
        // shape (Right(n)). v7.16.x will plumb the Engine's
        // existing Placeholder type-inference pass through.
        None
    }

    fn columns(&self) -> &[SpgColumn] {
        &self.columns
    }

    fn query(&self) -> sqlx_core::query::Query<'_, Spg, crate::SpgArguments<'_>> {
        sqlx_core::query::query_statement(self)
    }

    fn query_with<'s, A>(&'s self, arguments: A) -> sqlx_core::query::Query<'s, Spg, A>
    where
        A: sqlx_core::arguments::IntoArguments<'s, Spg>,
    {
        sqlx_core::query::query_statement_with(self, arguments)
    }

    fn query_as<O>(&self) -> sqlx_core::query_as::QueryAs<'_, Spg, O, crate::SpgArguments<'_>>
    where
        O: for<'r> sqlx_core::from_row::FromRow<'r, crate::SpgRow>,
    {
        sqlx_core::query_as::query_statement_as(self)
    }

    fn query_as_with<'s, O, A>(
        &'s self,
        arguments: A,
    ) -> sqlx_core::query_as::QueryAs<'s, Spg, O, A>
    where
        O: for<'r> sqlx_core::from_row::FromRow<'r, crate::SpgRow>,
        A: sqlx_core::arguments::IntoArguments<'s, Spg>,
    {
        sqlx_core::query_as::query_statement_as_with(self, arguments)
    }

    fn query_scalar<O>(
        &self,
    ) -> sqlx_core::query_scalar::QueryScalar<'_, Spg, O, crate::SpgArguments<'_>>
    where
        (O,): for<'r> sqlx_core::from_row::FromRow<'r, crate::SpgRow>,
    {
        sqlx_core::query_scalar::query_statement_scalar(self)
    }

    fn query_scalar_with<'s, O, A>(
        &'s self,
        arguments: A,
    ) -> sqlx_core::query_scalar::QueryScalar<'s, Spg, O, A>
    where
        (O,): for<'r> sqlx_core::from_row::FromRow<'r, crate::SpgRow>,
        A: sqlx_core::arguments::IntoArguments<'s, Spg>,
    {
        sqlx_core::query_scalar::query_statement_scalar_with(self, arguments)
    }
}

impl<'i> ColumnIndex<SpgStatement<'_>> for &'i str {
    fn index(&self, stmt: &SpgStatement<'_>) -> Result<usize, Error> {
        stmt.by_name
            .get(*self)
            .copied()
            .ok_or_else(|| Error::ColumnNotFound((*self).to_string()))
    }
}

impl ColumnIndex<SpgStatement<'_>> for usize {
    fn index(&self, stmt: &SpgStatement<'_>) -> Result<usize, Error> {
        if *self >= stmt.columns.len() {
            return Err(Error::ColumnIndexOutOfBounds {
                index: *self,
                len: stmt.columns.len(),
            });
        }
        Ok(*self)
    }
}
