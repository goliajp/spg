//! v7.16.0 — convert SPG engine errors into sqlx errors. The
//! adapter funnels every engine-side failure through this so
//! callers always see a `sqlx::Error`.

use std::sync::Arc;

use sqlx_core::error::{DatabaseError, Error, ErrorKind};

use spg_embedded::EngineError;

/// SPG-flavoured wrapper that surfaces an [`EngineError`]
/// through sqlx-core's [`DatabaseError`] trait, so `sqlx::Error`
/// preserves the engine-side text + kind classification.
#[derive(Debug)]
pub(crate) struct SpgDatabaseError {
    inner: EngineError,
}

impl SpgDatabaseError {
    pub(crate) fn new(inner: EngineError) -> Self {
        Self { inner }
    }
}

impl std::fmt::Display for SpgDatabaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.inner, f)
    }
}

impl std::error::Error for SpgDatabaseError {}

impl DatabaseError for SpgDatabaseError {
    fn message(&self) -> &str {
        // EngineError doesn't expose a fixed slice — we have to
        // store the formatted form somewhere persistent. For
        // v7.16.0 MVP, surface the variant name; the full
        // Display rendering still reaches callers through the
        // sqlx::Error Debug / Display.
        match self.inner {
            EngineError::Parse(_) => "parse error",
            EngineError::Storage(_) => "storage error",
            EngineError::Eval(_) => "eval error",
            EngineError::Unsupported(_) => "unsupported",
            EngineError::TransactionAlreadyOpen => "transaction already open",
            EngineError::NoActiveTransaction => "no active transaction",
            EngineError::WriteRequired => "write lock required",
            EngineError::RowLimitExceeded(_) => "row limit exceeded",
            EngineError::Cancelled => "cancelled",
            // v7.5.0 — EngineError is #[non_exhaustive]; any
            // future variant lands here as a generic engine
            // error pending a dedicated kind mapping.
            _ => "engine error",
        }
    }

    fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
        self
    }

    fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
        self
    }

    fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
        self
    }

    fn kind(&self) -> ErrorKind {
        // v7.16.0 — basic mapping. CHECK / NOT NULL / FK
        // violation classification will land alongside the per-
        // type Decode round-trip; for now everything is Other.
        ErrorKind::Other
    }
}

pub(crate) fn engine_to_sqlx(e: EngineError) -> Error {
    // Wrap the engine error in a DatabaseError so sqlx's
    // Error::Database variant carries it cleanly. sqlx-core
    // takes a `Box<dyn DatabaseError>` directly.
    let boxed: Box<dyn DatabaseError> = Box::new(SpgDatabaseError::new(e));
    Error::Database(boxed)
}
// Trick clippy / unused-import lint: Arc is no longer needed
// once the boxed wrap above stands on its own. Keep the import
// alias gated so future kind-mapping work can reach for Arc
// without re-importing.
#[allow(dead_code)]
type _ArcMarker = Arc<()>;
