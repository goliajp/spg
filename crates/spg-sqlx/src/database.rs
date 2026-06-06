//! v7.16.0 — the [`Spg`] marker type. Defines the 11 associated
//! types `sqlx::Database` requires for the adapter to plug into
//! every sqlx-core generic API (`query`, `query_as`, pool +
//! transaction lifecycle).

use sqlx_core::database::{Database, HasStatementCache};

use crate::{
    SpgArgumentValue, SpgArguments, SpgColumn, SpgConnection, SpgQueryResult, SpgRow, SpgStatement,
    SpgTransactionManager, SpgTypeInfo, SpgValue, SpgValueRef,
};

/// sqlx 0.8 driver for spg-embedded.
///
/// Used as the `<DB>` type parameter on every sqlx-core generic
/// API — e.g. `sqlx::query::<Spg>` is what
/// `sqlx::query("...").execute(&spg_pool)` collapses to.
///
/// Single-database — the adapter wraps one in-process
/// `AsyncDatabase` per [`SpgPool`][crate::SpgPool], with the
/// `Mutex` inside `AsyncDatabase` providing the serialised
/// engine-write invariant the spg-engine layer requires.
#[derive(Debug)]
pub struct Spg;

impl Database for Spg {
    type Connection = SpgConnection;
    type TransactionManager = SpgTransactionManager;
    type Row = SpgRow;
    type QueryResult = SpgQueryResult;
    type Column = SpgColumn;
    type TypeInfo = SpgTypeInfo;
    type Value = SpgValue;
    type ValueRef<'r> = SpgValueRef<'r>;
    type Arguments<'q> = SpgArguments<'q>;
    type ArgumentBuffer<'q> = Vec<SpgArgumentValue<'q>>;
    type Statement<'q> = SpgStatement<'q>;

    const NAME: &'static str = "SPG";
    const URL_SCHEMES: &'static [&'static str] = &["spg"];
}

// v7.16.0 — the engine plan cache lives inside spg-engine and
// is shared by every connection clone. Mark the driver so
// sqlx's prepared-statement reuse path opts in.
impl HasStatementCache for Spg {}
