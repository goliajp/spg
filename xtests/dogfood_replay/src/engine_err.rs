//! `EngineError` ↔ `anyhow::Error` adapter.
//!
//! `spg_engine::EngineError` implements `Display` but not
//! `std::error::Error`, so neither `?` nor `anyhow::Context` apply
//! to results coming back from the embedded API. This module
//! exposes a single helper used everywhere we touch the engine.

use spg_engine::EngineError;

/// Convert an `EngineError` into an `anyhow::Error` via its
/// `Display` impl.
pub fn ee(e: EngineError) -> anyhow::Error {
    anyhow::anyhow!("engine error: {e}")
}
