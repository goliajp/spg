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
