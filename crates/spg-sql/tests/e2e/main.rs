//! v7.20 test-speed A — spg-sql (perf_gate stays standalone: timing-sensitive) integration tests
//! merged into one target: one link instead of one per file,
//! libtest parallelises modules in-process.

mod corpus;
mod e2e_fk_parser;
mod e2e_on_conflict_parser;
mod e2e_table_constraints;
#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::uninlined_format_args,
    clippy::unusual_byte_groupings
)]
mod fuzz;
mod readonly_classifier;
