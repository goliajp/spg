//! v5.3 — `CatalogManifest` v10 file format.
//!
//! The manifest is the **single source of truth** for crash recovery
//! at scale. It captures three pieces of state that pre-v5.3 had to
//! be reconstructed by replaying the entire WAL on every boot:
//!
//! 1. **Hot-tier integrity hash** (`catalog_crc32`). The catalog
//!    snapshot at `<db_path>` is referenced by CRC32; a desync
//!    between manifest and snapshot is detected on load.
//! 2. **Cold-tier registry** (`cold_segments`). Each entry pins a
//!    `(segment_id, path, crc32)` triple so a v5.3 startup can
//!    rebuild `Catalog::cold_segments` automatically — no operator
//!    `SPG_PRELOAD_COLD_SEGMENT` env vars, no per-segment manual
//!    registration. v5.2's env-var path stays as a fallback.
//! 3. **WAL baseline offset** (`wal_baseline_offset`). The byte
//!    position in the WAL where replay must start. Bytes before
//!    this offset have already been incorporated into the catalog
//!    snapshot and can be truncated (v5.3.2). v5.0–v5.2 effectively
//!    treated this as 0 and replayed the WAL from scratch on every
//!    boot — which is what made 100M restarts take ~10 minutes
//!    instead of < 60 s.
//!
//! This module ships **the file format only** (v5.3.0): the type,
//! the serialiser, the deserialiser, and the error shape that
//! `STABILITY.md` will freeze under the v10 contract. Server
//! integration — writing the manifest on snapshot, reading it on
//! startup, truncating the WAL — lands in v5.3.1 / v5.3.2.
//!
//! ## Wire format
//!
//! ```text
//! [8 magic     = b"SPGMAN01"]
//! [1 version   = 10]
//! [4 catalog_crc32          ]  // CRC32 of the snapshot file at db_path
//! [4 num_segments           ]
//!   per segment:
//!     [4 segment_id         ]
//!     [4 path_byte_len      ]
//!     [N path_bytes (UTF-8) ]
//!     [4 segment_crc32      ]
//! [8 wal_baseline_offset    ]
//! [4 trailing_crc32         ]  // CRC32 over every byte above
//! ```
//!
//! All integers are little-endian (matching the rest of the SPG
//! on-disk surfaces). The trailing CRC32 covers every byte from the
//! magic up to (but not including) itself, so a one-bit flip
//! anywhere in the manifest surfaces as `ManifestError::BadCrc32`
//! at load time.

#![allow(clippy::module_name_repetitions)]
// v5.3.0 ships the format types in isolation so the bytes hit
// STABILITY.md before the integration commit. The serialize /
// deserialize sites land in v5.3.1; until then this whole
// module is reachable only from its own tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

const MANIFEST_MAGIC: &[u8; 8] = b"SPGMAN01";
const MANIFEST_VERSION: u8 = 10;
/// Header bytes before the per-segment array: magic + version +
/// `catalog_crc32` + `num_segments`. Used by length-validation
/// arithmetic in the deserializer.
const FIXED_HEADER_LEN: usize = 8 + 1 + 4 + 4;
/// Footer bytes after the per-segment array: `wal_baseline_offset` +
/// trailing CRC32.
const FOOTER_LEN: usize = 8 + 4;
/// Bytes per cold-segment entry header (`segment_id` + `path_len` +
/// `crc32`). The variable-length path bytes are not included here.
const SEGMENT_FIXED_LEN: usize = 4 + 4 + 4;

/// Frozen on-disk shape (v5.3 contract). See module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogManifest {
    /// CRC32 of the catalog snapshot bytes the manifest references.
    /// Detects desync between manifest and snapshot at load time.
    pub catalog_crc32: u32,
    /// Cold-tier segment registry. Order is preserved across
    /// round-trip; deserialise yields the same Vec the serialiser
    /// got.
    pub cold_segments: Vec<ColdSegmentEntry>,
    /// WAL replay starts from this byte offset. Bytes before this
    /// have been incorporated into the catalog snapshot and are
    /// candidates for `ftruncate` (v5.3.2).
    pub wal_baseline_offset: u64,
}

/// One row in the manifest's cold-tier registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColdSegmentEntry {
    /// Segment id allocated by `Catalog::load_segment_bytes` when
    /// the segment was first registered. Stable across restarts.
    pub segment_id: u32,
    /// Absolute path to the segment file. UTF-8 only — segments
    /// with non-UTF-8 paths can't ride the manifest path; operators
    /// fall back to `SPG_PRELOAD_COLD_SEGMENT` for those.
    pub path: PathBuf,
    /// CRC32 of the segment file contents at the time the manifest
    /// was written. Re-checked on load.
    pub crc32: u32,
}

/// Error shape surfaced by [`CatalogManifest::deserialize`]. The
/// variants are part of the v10 stability contract — adding a new
/// failure mode is a major bump.
#[derive(Debug, PartialEq, Eq)]
pub enum ManifestError {
    /// First 8 bytes weren't `SPGMAN01`.
    BadMagic { got: [u8; 8] },
    /// Version byte didn't match `MANIFEST_VERSION`. v10+1 readers
    /// will narrow this with a "supported range" check; v5.3.0 only
    /// accepts exactly 10.
    UnsupportedVersion { got: u8 },
    /// Manifest bytes stopped short of a structural offset. The
    /// `need` and `got` fields document the exact byte range the
    /// reader was expecting.
    Truncated {
        offset: usize,
        need: usize,
        got: usize,
    },
    /// Trailing CRC32 didn't match a fresh compute over the body.
    BadCrc32 { expected: u32, computed: u32 },
    /// A segment path byte slice wasn't valid UTF-8.
    PathNotUtf8 { segment_id: u32 },
    /// Trailing bytes after the footer. Manifest is appended-once;
    /// stray bytes mean the writer didn't finish or the file was
    /// corrupted post-write.
    TrailingBytes { unread: usize },
}

impl core::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadMagic { got } => write!(f, "manifest: bad magic {got:?}, expected SPGMAN01"),
            Self::UnsupportedVersion { got } => write!(
                f,
                "manifest: unsupported version {got}, this binary accepts only {MANIFEST_VERSION}"
            ),
            Self::Truncated { offset, need, got } => write!(
                f,
                "manifest: truncated at offset {offset}, need {need} bytes, got {got}"
            ),
            Self::BadCrc32 { expected, computed } => write!(
                f,
                "manifest: CRC32 mismatch (expected={expected:#010x}, computed={computed:#010x})"
            ),
            Self::PathNotUtf8 { segment_id } => write!(
                f,
                "manifest: cold-segment {segment_id} path is not valid UTF-8"
            ),
            Self::TrailingBytes { unread } => {
                write!(f, "manifest: {unread} trailing bytes after footer")
            }
        }
    }
}

impl std::error::Error for ManifestError {}

impl CatalogManifest {
    /// Pre-size a buffer for the wire format the current value
    /// would produce. Used by the serializer; exposed pub so the
    /// caller can avoid a Vec growth dance when streaming.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        let segs_bytes: usize = self
            .cold_segments
            .iter()
            .map(|s| SEGMENT_FIXED_LEN + s.path.as_os_str().len())
            .sum();
        FIXED_HEADER_LEN + segs_bytes + FOOTER_LEN
    }

    /// Encode the manifest to its v10 wire format. The output is
    /// what an operator's `ftruncate + rename` would land on disk
    /// at `<db>.spg/manifest.v10`.
    #[must_use]
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        out.extend_from_slice(MANIFEST_MAGIC);
        out.push(MANIFEST_VERSION);
        out.extend_from_slice(&self.catalog_crc32.to_le_bytes());
        let num = u32::try_from(self.cold_segments.len())
            .expect("manifest: cold segment count fits in u32");
        out.extend_from_slice(&num.to_le_bytes());
        for entry in &self.cold_segments {
            out.extend_from_slice(&entry.segment_id.to_le_bytes());
            let path_bytes = path_to_utf8(&entry.path);
            let path_len =
                u32::try_from(path_bytes.len()).expect("manifest: segment path length fits in u32");
            out.extend_from_slice(&path_len.to_le_bytes());
            out.extend_from_slice(path_bytes);
            out.extend_from_slice(&entry.crc32.to_le_bytes());
        }
        out.extend_from_slice(&self.wal_baseline_offset.to_le_bytes());
        // Trailing CRC32 over every byte appended so far. v4.37
        // backup bundle uses the same shape — keeps the corruption-
        // detection model consistent across SPG on-disk surfaces.
        let crc = spg_crypto::crc32::crc32(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    /// Parse a manifest from `bytes`. The body CRC32 is verified
    /// before any field is exposed to the caller, so a downstream
    /// `register_cold_segment(path)` can't be fed by a tampered
    /// path slice.
    pub fn deserialize(bytes: &[u8]) -> Result<Self, ManifestError> {
        // ---- length precheck: every fixed-size region must fit ----
        let min_len = FIXED_HEADER_LEN + FOOTER_LEN;
        if bytes.len() < min_len {
            return Err(ManifestError::Truncated {
                offset: 0,
                need: min_len,
                got: bytes.len(),
            });
        }
        // ---- magic + version ----
        let magic_slice = &bytes[..8];
        if magic_slice != MANIFEST_MAGIC {
            let mut got = [0u8; 8];
            got.copy_from_slice(magic_slice);
            return Err(ManifestError::BadMagic { got });
        }
        let version = bytes[8];
        if version != MANIFEST_VERSION {
            return Err(ManifestError::UnsupportedVersion { got: version });
        }
        // ---- trailing CRC32 over everything before the trailer ----
        let body_end = bytes.len() - 4;
        let trailing = u32::from_le_bytes([
            bytes[body_end],
            bytes[body_end + 1],
            bytes[body_end + 2],
            bytes[body_end + 3],
        ]);
        let computed = spg_crypto::crc32::crc32(&bytes[..body_end]);
        if trailing != computed {
            return Err(ManifestError::BadCrc32 {
                expected: trailing,
                computed,
            });
        }
        // ---- parse the body now that integrity is verified ----
        let catalog_crc32 = read_u32(bytes, 9);
        let num_segments = read_u32(bytes, 13) as usize;
        let mut offset = 17usize;
        let mut cold_segments = Vec::with_capacity(num_segments);
        for _ in 0..num_segments {
            // Per-segment header bounds-check.
            if offset + SEGMENT_FIXED_LEN > body_end {
                return Err(ManifestError::Truncated {
                    offset,
                    need: SEGMENT_FIXED_LEN,
                    got: body_end - offset,
                });
            }
            let segment_id = read_u32(bytes, offset);
            let path_len = read_u32(bytes, offset + 4) as usize;
            offset += 8;
            // Path body bounds-check.
            if offset + path_len + 4 > body_end {
                return Err(ManifestError::Truncated {
                    offset,
                    need: path_len + 4,
                    got: body_end - offset,
                });
            }
            let path_bytes = &bytes[offset..offset + path_len];
            let path_str = core::str::from_utf8(path_bytes)
                .map_err(|_| ManifestError::PathNotUtf8 { segment_id })?;
            offset += path_len;
            let crc32 = read_u32(bytes, offset);
            offset += 4;
            cold_segments.push(ColdSegmentEntry {
                segment_id,
                path: PathBuf::from(path_str),
                crc32,
            });
        }
        // ---- footer: wal_baseline_offset ----
        if offset + 8 > body_end {
            return Err(ManifestError::Truncated {
                offset,
                need: 8,
                got: body_end - offset,
            });
        }
        let wal_baseline_offset = u64::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ]);
        offset += 8;
        // After the footer u64 we should be sitting exactly on the
        // CRC32 trailer; anything else is corruption or truncation.
        if offset != body_end {
            return Err(ManifestError::TrailingBytes {
                unread: body_end.saturating_sub(offset),
            });
        }
        Ok(Self {
            catalog_crc32,
            cold_segments,
            wal_baseline_offset,
        })
    }
}

/// v5.3.1 — canonical sibling path for a db's manifest. Given
/// `<parent>/<stem>` (the catalog snapshot file), returns
/// `<parent>/<stem>.spg/manifest.v10`. The directory is the same
/// `.spg` folder the freezer writes cold-tier segments into, so a
/// single `<db>.spg/` tree captures everything v5.3+ persists
/// alongside the v9 catalog.
#[must_use]
pub fn manifest_path(db_path: &Path) -> PathBuf {
    let parent = db_path.parent().unwrap_or_else(|| Path::new("."));
    let stem = db_path
        .file_stem()
        .map_or_else(|| "db".to_string(), |s| s.to_string_lossy().into_owned());
    parent.join(format!("{stem}.spg")).join("manifest.v10")
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn path_to_utf8(p: &Path) -> &[u8] {
    // On Unix, OsStr -> bytes is infallible (via OsStrExt). For
    // portability the manifest format is documented as UTF-8 only;
    // unicode paths are universal on the platforms SPG runs on.
    p.as_os_str().to_str().map_or(b"", str::as_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CatalogManifest {
        CatalogManifest {
            catalog_crc32: 0xDEAD_BEEF,
            cold_segments: vec![
                ColdSegmentEntry {
                    segment_id: 0,
                    path: PathBuf::from("/var/lib/spg/db.spg/segments/seg_0.spg"),
                    crc32: 0x1111_1111,
                },
                ColdSegmentEntry {
                    segment_id: 1,
                    path: PathBuf::from("/var/lib/spg/db.spg/segments/seg_1.spg"),
                    crc32: 0x2222_2222,
                },
            ],
            wal_baseline_offset: 4_096 * 1024,
        }
    }

    #[test]
    fn round_trip_full_payload() {
        let m = sample();
        let bytes = m.serialize();
        assert_eq!(bytes.len(), m.encoded_len(), "encoded_len matches actual");
        let m2 = CatalogManifest::deserialize(&bytes).expect("round-trip parses");
        assert_eq!(m, m2);
    }

    #[test]
    fn round_trip_zero_segments() {
        let m = CatalogManifest {
            catalog_crc32: 0,
            cold_segments: vec![],
            wal_baseline_offset: 0,
        };
        let bytes = m.serialize();
        let m2 = CatalogManifest::deserialize(&bytes).expect("zero-segment manifest round-trips");
        assert_eq!(m, m2);
        // Layout assertion: 8 magic + 1 ver + 4 catalog_crc + 4 num
        //                 + 0 segments + 8 wal_offset + 4 crc = 29.
        assert_eq!(bytes.len(), FIXED_HEADER_LEN + FOOTER_LEN);
    }

    #[test]
    fn deserialize_rejects_bad_magic() {
        let mut bytes = sample().serialize();
        bytes[0] = b'X';
        match CatalogManifest::deserialize(&bytes) {
            Err(ManifestError::BadMagic { got }) => {
                assert_eq!(&got[..1], b"X");
            }
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[test]
    fn deserialize_rejects_unsupported_version() {
        let mut bytes = sample().serialize();
        bytes[8] = 99;
        // Recompute the CRC over the mutated body so the version
        // check fires before the CRC check.
        let body_end = bytes.len() - 4;
        let crc = spg_crypto::crc32::crc32(&bytes[..body_end]);
        bytes[body_end..].copy_from_slice(&crc.to_le_bytes());
        match CatalogManifest::deserialize(&bytes) {
            Err(ManifestError::UnsupportedVersion { got: 99 }) => {}
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn deserialize_rejects_corrupted_body() {
        let mut bytes = sample().serialize();
        // Flip one bit in the wal_baseline_offset region; the
        // trailing CRC32 must reject the manifest before any field
        // is exposed.
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0x01;
        match CatalogManifest::deserialize(&bytes) {
            Err(ManifestError::BadCrc32 { .. }) => {}
            other => panic!("expected BadCrc32, got {other:?}"),
        }
    }

    #[test]
    fn deserialize_rejects_truncated_input() {
        let bytes = sample().serialize();
        let truncated = &bytes[..bytes.len() - 8];
        // Last 4 bytes of `truncated` happen to be inside the body,
        // so the trailing-CRC validator will read them as the
        // "trailer" and compute a CRC over the wrong range. Either
        // BadCrc32 or Truncated is an acceptable outcome — the
        // contract is "any error, not a panic, not a successful
        // parse of garbage".
        let r = CatalogManifest::deserialize(truncated);
        assert!(
            matches!(
                r,
                Err(ManifestError::BadCrc32 { .. })
                    | Err(ManifestError::Truncated { .. })
                    | Err(ManifestError::TrailingBytes { .. })
                    | Err(ManifestError::PathNotUtf8 { .. })
            ),
            "expected an error, got {r:?}"
        );
    }

    #[test]
    fn deserialize_rejects_too_short_for_fixed_header() {
        let bytes = vec![0u8; FIXED_HEADER_LEN + FOOTER_LEN - 1];
        match CatalogManifest::deserialize(&bytes) {
            Err(ManifestError::Truncated { offset: 0, .. }) => {}
            other => panic!("expected Truncated, got {other:?}"),
        }
    }

    /// Pin the byte layout: a known fixed sample must serialise to
    /// exactly the documented number of bytes, and the magic /
    /// version / trailing CRC must sit at exact offsets the
    /// STABILITY.md documentation will freeze.
    #[test]
    fn wire_format_offsets_are_stable() {
        let m = sample();
        let bytes = m.serialize();
        // Magic at offset 0, exactly 8 bytes.
        assert_eq!(&bytes[..8], MANIFEST_MAGIC);
        // Version byte at offset 8.
        assert_eq!(bytes[8], MANIFEST_VERSION);
        // catalog_crc32 at offset 9..13 LE.
        assert_eq!(
            u32::from_le_bytes([bytes[9], bytes[10], bytes[11], bytes[12]]),
            m.catalog_crc32
        );
        // num_segments at offset 13..17 LE.
        assert_eq!(
            u32::from_le_bytes([bytes[13], bytes[14], bytes[15], bytes[16]]),
            m.cold_segments.len() as u32
        );
        // Trailing CRC32 is the last 4 bytes; reflects the actual
        // body so a re-compute matches.
        let body_end = bytes.len() - 4;
        let trailer = u32::from_le_bytes([
            bytes[body_end],
            bytes[body_end + 1],
            bytes[body_end + 2],
            bytes[body_end + 3],
        ]);
        assert_eq!(trailer, spg_crypto::crc32::crc32(&bytes[..body_end]));
    }
}
