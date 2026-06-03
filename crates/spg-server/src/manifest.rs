//! v7.1 — the manifest format moved into the standalone
//! `spg-manifest` crate so `spg-embedded` reads + writes the
//! same bytes. This shim re-exports the surface unchanged so
//! the rest of `spg-server` keeps using
//! `crate::manifest::{CatalogManifest, ColdSegmentEntry,
//! manifest_path, ManifestError}` exactly as before.

pub use spg_manifest::*;
