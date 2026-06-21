//! `EXPLAIN ANALYZE` parser + plan tree comparison.
//!
//! Stub for now. The mailrs-2026-06-22 content-worker fixture
//! records the EXPLAIN output as part of its README so an operator
//! can eyeball the plan shape, but the framework doesn't yet block
//! on plan-tree shape regressions — wall-time is the proxy.
//!
//! When we *do* start blocking on shape (catching e.g. "anti-join
//! degraded to nested-loop"), this module gets a real parser. The
//! API is shaped now so callers don't have to change later.

#![allow(dead_code)]

use crate::engine_err::ee;
use anyhow::Result;
use spg_embedded::Database;

/// Capture the EXPLAIN ANALYZE output for a SQL statement as plain
/// text. Wrapper over `db.execute("EXPLAIN ANALYZE …")` that
/// concatenates the result rows into one newline-joined blob.
pub fn capture_explain(db: &mut Database, sql: &str) -> Result<String> {
    let prefixed = if sql.trim_start().to_uppercase().starts_with("EXPLAIN") {
        sql.to_string()
    } else {
        format!("EXPLAIN ANALYZE {sql}")
    };
    let rows = db.query(&prefixed).map_err(ee)?;
    let mut out = String::new();
    for row in rows {
        for v in row {
            out.push_str(&format!("{:?}", v));
            out.push('\n');
        }
    }
    Ok(out)
}

/// Future: parse plan operators out of an EXPLAIN blob and return
/// a tree. For now a placeholder that returns the raw lines so
/// downstream callers have a stable signature.
pub fn parse_plan(blob: &str) -> Vec<String> {
    blob.lines().map(str::to_string).collect()
}
