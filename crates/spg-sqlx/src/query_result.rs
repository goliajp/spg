//! v7.16.0 — `sqlx::QueryResult` for SPG INSERT/UPDATE/DELETE.

/// Rows-affected counter returned by every DML execute. SPG
/// doesn't have last-insert-id (BIGSERIAL is computed inside
/// the engine + visible only via `RETURNING`); only the
/// affected-count is surfaced here.
#[derive(Debug, Default, Clone)]
pub struct SpgQueryResult {
    rows_affected: u64,
}

impl SpgQueryResult {
    /// Build a fresh result with the given rows-affected count.
    #[must_use]
    pub const fn new(rows_affected: u64) -> Self {
        Self { rows_affected }
    }

    /// Reads back the rows-affected count. Mirrors
    /// `sqlx::postgres::PgQueryResult::rows_affected`.
    #[must_use]
    pub const fn rows_affected(&self) -> u64 {
        self.rows_affected
    }
}

impl Extend<SpgQueryResult> for SpgQueryResult {
    fn extend<T: IntoIterator<Item = SpgQueryResult>>(&mut self, iter: T) {
        for next in iter {
            self.rows_affected = self.rows_affected.saturating_add(next.rows_affected);
        }
    }
}
