//! SPG SQL front-end. v0.2 ships only the lexer + a minimal recursive-descent
//! parser for the `SELECT [..] FROM [..] WHERE [..]` subset.
//!
//! Layers (each in its own module):
//!
//! - [`lexer`]  — byte stream → tokens
//! - [`ast`]    — abstract syntax tree types + `Display` (pretty-print)
//! - [`parser`] — tokens → AST (Pratt parser for expression precedence)
#![no_std]

extern crate alloc;

pub mod ast;
pub mod lexer;
pub mod parser;

/// v7.12.4 — convenience re-export of the PL/pgSQL body parser.
/// Used by the engine-side trigger executor to lazy-re-parse the
/// function body that the catalog stores as raw source text.
pub use parser::parse_function_body;

/// v7.37.14 (A2.5-stub) — process-wide counter of silent
/// FOR UPDATE / FOR SHARE / FOR KEY SHARE / FOR NO KEY UPDATE
/// clauses the parser accepted-and-discarded.
///
/// Pre-v7.37.15 the parser silently absorbs row-lock clauses so
/// mailrs / Rails / Django code paths that emit `SELECT … FOR
/// UPDATE` for advisory pessimistic locking load without a parser
/// error. The clauses are not enforced — SPG is currently single-
/// writer + Arc snapshot,which already satisfies the implicit
/// ordering most callers want.
///
/// v7.37.15 (B2.5 / fine-grained MVCC) will land per-row tuple
/// locking and start honouring these clauses. Until then, this
/// counter is the *observability hook* so operators can surface
/// "FOR UPDATE is widely used in this workload — once 7.37.15
/// ships, ensure the application semantics are still correct".
///
/// Bumped once per FOR clause consumed (so `FOR UPDATE OF t1 FOR
/// SHARE OF t2` increments by 2). Reads via [`silent_for_update_count`].
static SILENT_FOR_UPDATE_COUNT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// v7.37.14 (A2.5-stub) — bump the silent-FOR-UPDATE counter.
/// Called from the parser's `consume_optional_for_lock_clauses`
/// path. Public-but-low-traffic API; not part of the stable parser
/// surface.
#[doc(hidden)]
pub fn record_silent_for_update_clause() {
    SILENT_FOR_UPDATE_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// v7.37.14 (A2.5-stub) — read the process-wide silent-FOR-UPDATE
/// counter. Engines / spgctl / monitoring use this to surface
/// "how many advisory row locks did the workload ask for since
/// process start". Returns 0 if no FOR UPDATE / FOR SHARE clause
/// has hit the parser yet.
#[must_use]
pub fn silent_for_update_count() -> u64 {
    SILENT_FOR_UPDATE_COUNT.load(core::sync::atomic::Ordering::Relaxed)
}
