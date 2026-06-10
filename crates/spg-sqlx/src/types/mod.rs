//! v7.16.0 — `Type` + `Encode` + `Decode` for the basic scalar
//! surface. mailrs's most-used columns are INT / BIGINT / TEXT /
//! BOOLEAN / BYTEA / TIMESTAMPTZ / JSONB — this module ships
//! the first five; the date/time + JSON families land in
//! follow-up commits.

mod array;
mod bool;
mod bytes;
#[cfg(feature = "chrono")]
mod chrono;
pub(crate) mod decimal;
mod float;
mod int;
#[cfg(feature = "json")]
mod json;
mod text;
pub(crate) mod uuid;
pub(crate) mod vector;

// `Option<T>` binds → SQL NULL when None. sqlx-core's Encode-for-Option
// is a per-driver macro, NOT a blanket impl — every Database driver has
// to invoke it itself (sqlx-postgres does the same in its lib.rs).
// Without this every `.bind(Option<...>)` resolves to the Postgres
// driver and fails the executor type-check (mailrs embed round-12).
use crate::database::Spg;
sqlx_core::impl_encode_for_option!(Spg);
