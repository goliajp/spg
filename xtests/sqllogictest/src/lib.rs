// xtests crate — dev-only baselining tool, not shipped in the binary. Loosen
// clippy::pedantic carve-outs that aren't worth carrying for an internal
// harness; correctness of the harness itself is what matters.
#![allow(
    clippy::doc_markdown,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::used_underscore_binding,
    clippy::nonminimal_bool,
    clippy::if_same_then_else,
    clippy::match_same_arms,
    clippy::match_wildcard_for_single_variants,
    clippy::too_many_lines,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::needless_pass_by_value,
    clippy::wildcard_imports,
    clippy::format_push_string
)]

//! Self-built sqllogictest parser + runner for SPG conformance baselining.
//!
//! Format reference: <https://www.sqlite.org/sqllogictest/doc/trunk/about.wiki>
//!
//! We implement the subset that DuckDB and CockroachDB actually emit in their
//! corpora; the obscure record types (`hash-threshold`, `loop`, `mode`) are
//! parsed-and-ignored or rejected with a clear "skip" reason. The runner
//! drives an in-process `spg_engine::Engine` — fast (no socket) and removes
//! the daemon as a moving part during conformance work.

pub mod parser;
pub mod runner;

pub use parser::{Directive, ExpectedQuery, Record, parse_file};
pub use runner::{Outcome, RunOutcome, Runner};
