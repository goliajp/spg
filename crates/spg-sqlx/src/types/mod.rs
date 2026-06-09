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
