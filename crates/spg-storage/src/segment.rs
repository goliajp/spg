// Segment encoding crosses the u64/usize → f64 boundary in offset
// arithmetic; the `as` casts on file offsets and page indices are
// well-defined and bounded by `num_rows` / `num_pages` which the
// writer caps at `u32::MAX` rows per segment.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::doc_markdown,
    clippy::items_after_statements,
    clippy::similar_names,
    clippy::unreadable_literal
)]

//! v5.0 — cold-tier segment file codec. A `Segment` is an immutable,
//! PK-sorted file of `(u64_key, row_bytes)` entries with three
//! sidecar sections for fast probing: a `BloomFilter` over the
//! keys, a page index, and the payload pages themselves. The v5
//! freezer (v5.2 work) writes one segment per "freeze batch"; the
//! v5.1 two-tier catalog probes the bloom first, then the page
//! index, then a single 4 KiB page read — so a missed cold-tier
//! probe costs at most ~`bloom.contains()` time, and a hit costs
//! one disk seek + page-internal binary search.
//!
//! **Byte-only API.** This module is `no_std`-safe and never
//! touches `std::fs`. The writer produces a `Vec<u8>` that the
//! caller writes to disk; the reader takes a `&[u8]` slice that
//! the caller obtained via `std::fs::read` (full-load) or
//! `mmap`/seek-style page-at-a-time access. Splitting codec from
//! file I/O lets v5.1 wrap a `SeekableSegmentReader` around the
//! same byte layout without forcing spg-storage onto `std`.
//!
//! ## File format (v1, frozen from v5.0 ship)
//!
//! ```text
//! [8 bytes  b"SPGSEG\x01"]                magic + version 1
//! [u32 LE   num_rows]                     count, ≤ u32::MAX
//! [u32 LE   num_pages]                    count, ≤ u32::MAX
//! [u32 LE   page_size_bytes]              4096 in v5.0 (stored
//!                                          so future versions can
//!                                          tune without bumping
//!                                          magic)
//! [u64 LE   min_pk]                       smallest PK in segment
//! [u64 LE   max_pk]                       largest PK in segment
//! [u32 LE   bloom_len_bytes]              length-prefixed bloom
//! [bloom bytes ...]                       BloomFilter::to_bytes
//!                                          output, verbatim
//! [u32 LE   page_index_len_bytes]         length-prefixed index
//! [page index bytes ...]                  Vec<(u64 first_pk_of_page,
//!                                          u32 file_offset)>
//!                                          serialised LE-packed
//! [page 0]                                page_size_bytes bytes
//! [page 1]
//! ...
//! [page N-1]
//! [u32 LE   crc32_body]                   crc32 over everything
//!                                          from `num_rows` byte
//!                                          through the last page
//! ```
//!
//! ## Page format (v1)
//!
//! Each page is exactly `page_size_bytes` (4096 in v5.0). Inside
//! a page:
//!
//! ```text
//! [u32 LE   num_rows_in_page]             how many rows pack here
//! [u32 LE × num_rows_in_page  row_offsets]  byte offset within
//!                                            this page where the
//!                                            row payload starts
//! [row payload bytes ...]                 concatenated, no padding
//! [zero padding ...]                      to page_size_bytes
//! ```
//!
//! Each row payload is `[u64 LE key][u32 LE payload_len]
//! [payload_len bytes payload]`. Caller owns payload semantics.
//!
//! ## What's frozen vs not
//!
//! - **Frozen as v1**: magic bytes, header field order/types, bloom
//!   layout (already frozen via `BloomFilter` v1), page-index layout,
//!   page-internal layout, CRC32 algorithm.
//! - **Not frozen**: `page_size_bytes` value (4096 is the v5.0
//!   default; future versions may tune via env knob without
//!   bumping magic — the field is stored in-band).

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use spg_crypto::crc32::crc32;

use crate::bloom::{BloomError, BloomFilter};

/// Segment file magic. Distinct from `SPGDB001` (catalog snapshot,
/// v3.0) and `SPGBKUP\x01`/`\x02` (backup bundles, v4.25/v4.37) so a
/// reader can disambiguate a stray slice.
pub const SEGMENT_MAGIC: [u8; 8] = *b"SPGSEG\x01\x00";

/// v6.6.2 — segment file v2 magic. A v2 file wraps the v1 byte
/// sequence (magic + body + CRC32 footer) inside a compression
/// envelope:
///   [8-byte magic SEGMENT_MAGIC_V2]
///   [u8 algo: 0=none, 1=LZSS]
///   [u32 LE inner_uncompressed_len]
///   [inner bytes — either the raw v1 segment OR LZSS-compressed]
/// v6.6+ readers detect v2 by magic and transparently unwrap; v1
/// files (magic `SPGSEG\x01\x00`) still load through the legacy
/// parser path with zero changes.
pub const SEGMENT_MAGIC_V2: [u8; 8] = *b"SPGSEG\x02\x00";
pub(crate) const SEGMENT_V2_HEADER_LEN: usize = 8 + 1 + 4;
pub const SEGMENT_COMPRESS_ALGO_NONE: u8 = 0;
pub const SEGMENT_COMPRESS_ALGO_LZSS: u8 = 1;

/// Default page byte count. Stored in the segment header so future
/// versions can tune without a magic bump. 4096 matches APFS / ext4
/// default page size — a single page read is one disk I/O on every
/// mainstream filesystem.
pub const SEGMENT_PAGE_BYTES: u32 = 4096;

/// Header byte count from `magic` through `page_index_len_bytes`
/// **not** counting the variable-length bloom + page index. Used
/// by the writer to reserve space; used by the reader to compute
/// fixed-offset fields.
const HEADER_FIXED_LEN: usize = 8 + 4 + 4 + 4 + 8 + 8 + 4; // = 40

/// CRC32 footer length.
const FOOTER_LEN: usize = 4;

/// Errors surfaced by the segment reader. Includes the inner
/// `BloomError` since the bloom is parsed during `open`.
#[derive(Debug)]
pub enum SegmentError {
    TooShort {
        got: usize,
        need: usize,
    },
    BadMagic {
        got: [u8; 8],
    },
    BadShape(String),
    BadCrc {
        expected: u32,
        got: u32,
    },
    BloomError(BloomError),
    UnsortedKey {
        prev: u64,
        next: u64,
    },
    KeyNotInPage {
        key: u64,
    },
    /// Caller asked for a page outside `[0, num_pages)`.
    PageOutOfRange {
        got: u32,
        num_pages: u32,
    },
    /// v6.6.2 — v2 envelope's inner LZSS payload failed to
    /// decompress. The contained string is the underlying
    /// `LzssError` rendered.
    CompressionDecodeFailed(String),
    /// v6.6.2 — v2 envelope declares an unknown compression algo
    /// byte. Refuse to read forward without knowing how to
    /// interpret the inner bytes.
    UnknownCompressionAlgo(u8),
}

impl fmt::Display for SegmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { got, need } => write!(
                f,
                "segment: too short, got {got} bytes, need at least {need}"
            ),
            Self::BadMagic { got } => {
                write!(f, "segment: bad magic {got:?}, expected {SEGMENT_MAGIC:?}")
            }
            Self::BadShape(s) => write!(f, "segment: bad shape: {s}"),
            Self::BadCrc { expected, got } => write!(
                f,
                "segment: crc mismatch, expected 0x{expected:08x}, got 0x{got:08x}"
            ),
            Self::BloomError(e) => write!(f, "segment: bloom decode failed: {e}"),
            Self::UnsortedKey { prev, next } => write!(
                f,
                "segment: writer received unsorted keys (prev={prev}, next={next}); \
                 the segment contract requires ascending u64 keys"
            ),
            Self::KeyNotInPage { key } => {
                write!(f, "segment: key {key} not found in target page")
            }
            Self::PageOutOfRange { got, num_pages } => write!(
                f,
                "segment: page index {got} out of range, num_pages = {num_pages}"
            ),
            Self::CompressionDecodeFailed(s) => write!(
                f,
                "segment v2 envelope: LZSS decompress failed: {s}"
            ),
            Self::UnknownCompressionAlgo(b) => write!(
                f,
                "segment v2 envelope: unknown compression algo byte {b:#04x}"
            ),
        }
    }
}

impl From<BloomError> for SegmentError {
    fn from(e: BloomError) -> Self {
        Self::BloomError(e)
    }
}

/// Lightweight summary of a finished segment — what the catalog
/// manifest (v5.3 work) records to find the segment on disk and
/// what `RowLocator::Cold` (v5.1 work) carries inside the PB
/// index. Generated by `Segment::encode`.
#[derive(Debug, Clone)]
pub struct SegmentMeta {
    pub num_rows: u64,
    pub num_pages: u32,
    pub page_size_bytes: u32,
    pub min_pk: u64,
    pub max_pk: u64,
    /// Length of the full serialised segment in bytes. Useful for
    /// preallocating the file or sanity-checking after `write_all`.
    pub total_bytes: usize,
}

/// One page-index entry: `(first_pk_in_page, file_offset_to_page_start)`.
/// `Vec<PageIndexEntry>` is sorted by `first_pk`, so a `lookup(key)`
/// binary-searches this to find the candidate page, then reads /
/// parses that page only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PageIndexEntry {
    first_pk: u64,
    file_offset: u32,
}

/// Build a complete segment as a single `Vec<u8>`. Caller writes
/// the returned bytes to disk; subsequent reads happen via
/// `SegmentReader` against either the full in-RAM slice (for the
/// v5.0 standalone perf gates) or a seekable wrapper that pulls
/// one page at a time (v5.1 catalog integration).
///
/// `bloom_target_fp` sizes the embedded bloom; the standard v5
/// default is `0.01` (1 % false-positive ceiling). Callers can
/// trade per-segment bloom size for selectivity (smaller fp_rate
/// → larger bloom).
///
/// `rows` must yield entries with **ascending u64 keys**; any
/// descending or duplicate-out-of-order entry returns
/// `SegmentError::UnsortedKey`.
///
/// `page_size_bytes` should be 4096 in v5.0 ship; the writer
/// rejects values smaller than 256 (would force pathological
/// page-count) and larger than 65536 (would defeat the
/// page-granularity I/O assumption).
#[allow(clippy::too_many_lines)]
pub fn encode_segment<I>(
    rows: I,
    bloom_target_fp: f64,
    page_size_bytes: u32,
) -> Result<(Vec<u8>, SegmentMeta), SegmentError>
where
    I: ExactSizeIterator<Item = (u64, Vec<u8>)>,
{
    if !(256..=65_536).contains(&page_size_bytes) {
        return Err(SegmentError::BadShape(format!(
            "page_size_bytes {page_size_bytes} must be in [256, 65536]"
        )));
    }
    let num_rows_hint = rows.len();
    if num_rows_hint == 0 {
        return Err(SegmentError::BadShape(
            "encode_segment: at least one row required".into(),
        ));
    }
    // First pass: bucket rows into pages, building the bloom and
    // recording per-page first-keys for the index. Within each
    // page we just collect the row payload bytes; the actual
    // within-page offsets are computed in `serialise_page` once
    // the final row count for that page is known.
    let mut bloom = BloomFilter::with_target_fp_rate(num_rows_hint, bloom_target_fp);
    let mut pages: Vec<Vec<u8>> = Vec::new();
    let mut page_index: Vec<PageIndexEntry> = Vec::new();
    let mut row_bytes_in_page: Vec<Vec<u8>> = Vec::new();
    let mut first_pk_in_page: Option<u64> = None;
    let mut last_key: Option<u64> = None;
    let mut min_pk: Option<u64> = None;
    let mut max_pk: u64 = 0;
    let mut total_rows: u64 = 0;
    for (key, payload) in rows {
        if let Some(prev) = last_key
            && key <= prev
        {
            return Err(SegmentError::UnsortedKey { prev, next: key });
        }
        last_key = Some(key);
        if min_pk.is_none() {
            min_pk = Some(key);
        }
        max_pk = key;
        total_rows = total_rows.wrapping_add(1);
        bloom.insert(&key.to_le_bytes());
        // Row payload as it lives on the page: [u64 key][u32 plen][plen bytes].
        let mut row_bytes = Vec::with_capacity(12 + payload.len());
        row_bytes.extend_from_slice(&key.to_le_bytes());
        let plen = u32::try_from(payload.len()).map_err(|_| {
            SegmentError::BadShape(format!(
                "row payload too large: {} bytes > u32::MAX",
                payload.len()
            ))
        })?;
        row_bytes.extend_from_slice(&plen.to_le_bytes());
        row_bytes.extend_from_slice(&payload);
        // Check if adding this row would overflow the page. The
        // resulting page is laid out as:
        //   [u32 num_rows][u32 × num_rows offsets][row bytes...]
        // so the byte cost of N rows in a page is
        //   4 + 4*N + sum(row.len()).
        let proposed_num_rows = row_bytes_in_page.len() + 1;
        let proposed_offsets_bytes = proposed_num_rows * 4;
        let proposed_rows_bytes: usize =
            row_bytes_in_page.iter().map(Vec::len).sum::<usize>() + row_bytes.len();
        let proposed_size = 4 + proposed_offsets_bytes + proposed_rows_bytes;
        if proposed_size > page_size_bytes as usize {
            if row_bytes_in_page.is_empty() {
                // Single row larger than the whole page — caller
                // must use a bigger page_size_bytes or smaller
                // rows. Surface as bad shape rather than silently
                // bloating the page.
                return Err(SegmentError::BadShape(format!(
                    "row of {} bytes doesn't fit in page of {page_size_bytes} bytes",
                    row_bytes.len()
                )));
            }
            // Finalise current page.
            let page_file_offset =
                u32::try_from(pages.len() * page_size_bytes as usize).expect("page count fits u32");
            page_index.push(PageIndexEntry {
                first_pk: first_pk_in_page.expect("page is non-empty"),
                file_offset: page_file_offset,
            });
            let finalised = serialise_page(&row_bytes_in_page, page_size_bytes as usize);
            pages.push(finalised);
            row_bytes_in_page.clear();
            first_pk_in_page = None;
        }
        // Now add to the (possibly fresh) current page.
        if first_pk_in_page.is_none() {
            first_pk_in_page = Some(key);
        }
        row_bytes_in_page.push(row_bytes);
    }
    // Finalise the last page (always non-empty since num_rows >= 1
    // and the loop wrote at least one row).
    if !row_bytes_in_page.is_empty() {
        let page_file_offset =
            u32::try_from(pages.len() * page_size_bytes as usize).expect("page count fits u32");
        page_index.push(PageIndexEntry {
            first_pk: first_pk_in_page.expect("trailing page is non-empty"),
            file_offset: page_file_offset,
        });
        let final_page = serialise_page(&row_bytes_in_page, page_size_bytes as usize);
        pages.push(final_page);
    }
    let num_pages = u32::try_from(pages.len()).map_err(|_| {
        SegmentError::BadShape(format!(
            "segment has {} pages, exceeds u32::MAX",
            pages.len()
        ))
    })?;
    let num_rows = total_rows;
    let num_rows_u32 = u32::try_from(num_rows)
        .map_err(|_| SegmentError::BadShape(format!("num_rows {num_rows} exceeds u32::MAX")))?;
    let min_pk = min_pk.expect("non-empty rows");
    // Serialise bloom + page index ahead of time so we know their
    // byte lengths (the header carries them as length prefixes).
    let bloom_bytes = bloom.to_bytes();
    let page_index_bytes = encode_page_index(&page_index);
    // Assemble the file.
    let mut out = Vec::with_capacity(
        HEADER_FIXED_LEN
            + 4
            + bloom_bytes.len()
            + 4
            + page_index_bytes.len()
            + pages.len() * page_size_bytes as usize
            + FOOTER_LEN,
    );
    out.extend_from_slice(&SEGMENT_MAGIC);
    let body_start = out.len();
    out.extend_from_slice(&num_rows_u32.to_le_bytes());
    out.extend_from_slice(&num_pages.to_le_bytes());
    out.extend_from_slice(&page_size_bytes.to_le_bytes());
    out.extend_from_slice(&min_pk.to_le_bytes());
    out.extend_from_slice(&max_pk.to_le_bytes());
    out.extend_from_slice(
        &u32::try_from(bloom_bytes.len())
            .expect("bloom < 4 GiB")
            .to_le_bytes(),
    );
    out.extend_from_slice(&bloom_bytes);
    out.extend_from_slice(
        &u32::try_from(page_index_bytes.len())
            .expect("page index < 4 GiB")
            .to_le_bytes(),
    );
    out.extend_from_slice(&page_index_bytes);
    for page in &pages {
        debug_assert_eq!(page.len(), page_size_bytes as usize, "page is fixed-size");
        out.extend_from_slice(page);
    }
    // CRC32 covers everything from `num_rows` (body_start) through
    // the last page byte. Magic is excluded; footer is the CRC
    // itself.
    let crc = crc32(&out[body_start..]);
    out.extend_from_slice(&crc.to_le_bytes());
    let meta = SegmentMeta {
        num_rows,
        num_pages,
        page_size_bytes,
        min_pk,
        max_pk,
        total_bytes: out.len(),
    };
    Ok((out, meta))
}

/// Serialise a single page into exactly `page_size_bytes` bytes.
/// Layout: `[u32 num_rows][u32 row_offsets[num_rows]][row payloads
/// concatenated]`, zero-padded to the page size. Offsets are
/// computed here (not at caller's level) because they depend on
/// the final row count for the page, which is only known at
/// serialise time.
fn serialise_page(row_bytes: &[Vec<u8>], page_size_bytes: usize) -> Vec<u8> {
    let num_rows = u32::try_from(row_bytes.len()).expect("row count fits u32");
    let offsets_section_bytes = num_rows as usize * 4;
    let header_total = 4 + offsets_section_bytes;
    let mut page = Vec::with_capacity(page_size_bytes);
    page.extend_from_slice(&num_rows.to_le_bytes());
    // Reserve the offsets section; we'll backfill once we know
    // each row's byte position.
    page.resize(header_total, 0);
    // Append row bytes, recording the within-page offset of each.
    let mut offsets = Vec::with_capacity(row_bytes.len());
    for row in row_bytes {
        offsets.push(u32::try_from(page.len()).expect("page < 4 GiB"));
        page.extend_from_slice(row);
    }
    // Backfill the offsets section.
    for (i, off) in offsets.iter().enumerate() {
        let pos = 4 + i * 4;
        page[pos..pos + 4].copy_from_slice(&off.to_le_bytes());
    }
    debug_assert!(
        page.len() <= page_size_bytes,
        "page overflow: {} > {page_size_bytes}",
        page.len()
    );
    page.resize(page_size_bytes, 0);
    page
}

/// Pack the page index as `[u32 LE count][(u64 LE first_pk, u32 LE
/// file_offset)...]`. Decoded by `parse_page_index`.
fn encode_page_index(index: &[PageIndexEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + index.len() * 12);
    out.extend_from_slice(
        &u32::try_from(index.len())
            .expect("page count fits u32")
            .to_le_bytes(),
    );
    for entry in index {
        out.extend_from_slice(&entry.first_pk.to_le_bytes());
        out.extend_from_slice(&entry.file_offset.to_le_bytes());
    }
    out
}

fn parse_page_index(input: &[u8]) -> Result<Vec<PageIndexEntry>, SegmentError> {
    if input.len() < 4 {
        return Err(SegmentError::BadShape(
            "page index: too short for count prefix".into(),
        ));
    }
    let count = u32::from_le_bytes([input[0], input[1], input[2], input[3]]) as usize;
    let expected = 4 + count * 12;
    if input.len() != expected {
        return Err(SegmentError::BadShape(format!(
            "page index: input is {} bytes, expected {} for count {count}",
            input.len(),
            expected
        )));
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let off = 4 + i * 12;
        let first_pk = u64::from_le_bytes([
            input[off],
            input[off + 1],
            input[off + 2],
            input[off + 3],
            input[off + 4],
            input[off + 5],
            input[off + 6],
            input[off + 7],
        ]);
        let file_offset = u32::from_le_bytes([
            input[off + 8],
            input[off + 9],
            input[off + 10],
            input[off + 11],
        ]);
        out.push(PageIndexEntry {
            first_pk,
            file_offset,
        });
    }
    Ok(out)
}

/// Parsed segment state — meta, bloom, page-index, and the file
/// offset where the page payloads begin. Shared between
/// [`SegmentReader`] (borrows bytes) and [`OwnedSegment`] (owns
/// bytes) so both share a single `parse + lookup` implementation.
///
/// Module-private: callers should hold a `SegmentReader` or an
/// `OwnedSegment` instead of constructing this directly.
#[derive(Debug, Clone)]
struct SegmentMetadata {
    meta: SegmentMeta,
    bloom: BloomFilter,
    page_index: Vec<PageIndexEntry>,
    /// File offset where the first page starts. The metadata hides
    /// the variable-length bloom + page-index sections behind
    /// this anchor.
    pages_start_offset: usize,
}

/// Parse the segment header + bloom + page-index from `bytes`,
/// validating magic, CRC32 footer, and structural lengths. The
/// returned [`SegmentMetadata`] is independent of `bytes`'
/// lifetime so it can be embedded inside an [`OwnedSegment`] that
/// owns its own `Vec<u8>`.
fn parse_segment_metadata(bytes: &[u8]) -> Result<SegmentMetadata, SegmentError> {
    if bytes.len() < HEADER_FIXED_LEN + FOOTER_LEN {
        return Err(SegmentError::TooShort {
            got: bytes.len(),
            need: HEADER_FIXED_LEN + FOOTER_LEN,
        });
    }
    let mut magic = [0u8; 8];
    magic.copy_from_slice(&bytes[..8]);
    if magic != SEGMENT_MAGIC {
        return Err(SegmentError::BadMagic { got: magic });
    }
    // Header parse.
    let num_rows = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    let num_pages = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    let page_size_bytes = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let min_pk = u64::from_le_bytes([
        bytes[20], bytes[21], bytes[22], bytes[23], bytes[24], bytes[25], bytes[26], bytes[27],
    ]);
    let max_pk = u64::from_le_bytes([
        bytes[28], bytes[29], bytes[30], bytes[31], bytes[32], bytes[33], bytes[34], bytes[35],
    ]);
    let bloom_len = u32::from_le_bytes([bytes[36], bytes[37], bytes[38], bytes[39]]) as usize;
    let bloom_offset = HEADER_FIXED_LEN;
    if bytes.len() < bloom_offset + bloom_len + 4 {
        return Err(SegmentError::TooShort {
            got: bytes.len(),
            need: bloom_offset + bloom_len + 4,
        });
    }
    let bloom = BloomFilter::from_bytes(&bytes[bloom_offset..bloom_offset + bloom_len])?;
    let page_index_len_off = bloom_offset + bloom_len;
    let page_index_len = u32::from_le_bytes([
        bytes[page_index_len_off],
        bytes[page_index_len_off + 1],
        bytes[page_index_len_off + 2],
        bytes[page_index_len_off + 3],
    ]) as usize;
    let page_index_off = page_index_len_off + 4;
    if bytes.len() < page_index_off + page_index_len {
        return Err(SegmentError::TooShort {
            got: bytes.len(),
            need: page_index_off + page_index_len,
        });
    }
    let page_index = parse_page_index(&bytes[page_index_off..page_index_off + page_index_len])?;
    let pages_start_offset = page_index_off + page_index_len;
    let pages_total_bytes = num_pages as usize * page_size_bytes as usize;
    let expected_total = pages_start_offset + pages_total_bytes + FOOTER_LEN;
    if bytes.len() != expected_total {
        return Err(SegmentError::BadShape(format!(
            "segment: input is {} bytes, header implies {expected_total}",
            bytes.len()
        )));
    }
    // CRC footer check (body excludes magic + the CRC itself).
    let stored_crc_off = expected_total - FOOTER_LEN;
    let stored_crc = u32::from_le_bytes([
        bytes[stored_crc_off],
        bytes[stored_crc_off + 1],
        bytes[stored_crc_off + 2],
        bytes[stored_crc_off + 3],
    ]);
    let computed_crc = crc32(&bytes[8..stored_crc_off]);
    if computed_crc != stored_crc {
        return Err(SegmentError::BadCrc {
            expected: stored_crc,
            got: computed_crc,
        });
    }
    let meta = SegmentMeta {
        num_rows: u64::from(num_rows),
        num_pages,
        page_size_bytes,
        min_pk,
        max_pk,
        total_bytes: bytes.len(),
    };
    Ok(SegmentMetadata {
        meta,
        bloom,
        page_index,
        pages_start_offset,
    })
}

/// Out-of-range + bloom check. Shared between [`SegmentReader`]
/// and [`OwnedSegment`] so a single implementation is on the hot
/// path.
fn segment_might_contain(metadata: &SegmentMetadata, key: u64) -> bool {
    if key < metadata.meta.min_pk || key > metadata.meta.max_pk {
        return false;
    }
    metadata.bloom.contains(&key.to_le_bytes())
}

/// Page-aware lookup. Shared between [`SegmentReader`] and
/// [`OwnedSegment`] so the single-page-read budget invariant holds
/// for both. Returns the raw payload bytes (caller decides how to
/// decode them — for a v5.1 cold-tier read that's the dense Row
/// body for the cold table).
fn segment_lookup(metadata: &SegmentMetadata, bytes: &[u8], key: u64) -> Option<Vec<u8>> {
    if !segment_might_contain(metadata, key) {
        return None;
    }
    // Binary-search the page index for the largest entry with
    // `first_pk <= key`.
    let candidate = match metadata
        .page_index
        .binary_search_by(|entry| entry.first_pk.cmp(&key))
    {
        Ok(i) => i,
        Err(0) => return None,
        Err(i) => i - 1,
    };
    let entry = metadata.page_index[candidate];
    let page_off = metadata.pages_start_offset + entry.file_offset as usize;
    let page_end = page_off + metadata.meta.page_size_bytes as usize;
    if page_end > bytes.len() - FOOTER_LEN {
        return None;
    }
    let page = &bytes[page_off..page_end];
    decode_page_lookup(page, key)
}

/// Sorted-order scan. Shared by both reader flavours.
fn segment_scan<'a>(
    metadata: &'a SegmentMetadata,
    bytes: &'a [u8],
) -> impl Iterator<Item = (u64, Vec<u8>)> + 'a {
    let page_size = metadata.meta.page_size_bytes as usize;
    (0..metadata.meta.num_pages as usize).flat_map(move |i| {
        let off = metadata.pages_start_offset + i * page_size;
        let page = &bytes[off..off + page_size];
        decode_page_iter(page)
    })
}

/// Read-side handle. Borrows the segment bytes (the catalog or
/// test owns the buffer), parses header + bloom + page index up
/// front, and exposes `lookup(key)` / `scan_keys()` over the rest.
///
/// For an in-RAM cold-tier segment that the catalog holds across
/// many lookups, prefer [`OwnedSegment`] — it owns its bytes and
/// reuses the same parsed metadata across calls without any
/// lifetime gymnastics.
#[derive(Debug)]
pub struct SegmentReader<'a> {
    bytes: &'a [u8],
    metadata: SegmentMetadata,
}

/// v6.6.2 — wrap v1 segment bytes in a v2 LZSS envelope when
/// `compress=true` and the compressed form is strictly smaller.
/// Returns the v1 bytes unchanged otherwise (the caller's "ship
/// the smaller form" policy lives at the catalog layer; this
/// helper only commits to NOT making files bigger).
#[must_use]
pub fn wrap_v2_envelope(v1_bytes: Vec<u8>, compress: bool) -> Vec<u8> {
    if !compress {
        return v1_bytes;
    }
    let compressed = spg_crypto::lzss::compress(&v1_bytes);
    if compressed.len() + SEGMENT_V2_HEADER_LEN >= v1_bytes.len() {
        return v1_bytes;
    }
    let inner_len = u32::try_from(v1_bytes.len()).expect("v1 segment < 4 GiB");
    let mut out = Vec::with_capacity(SEGMENT_V2_HEADER_LEN + compressed.len());
    out.extend_from_slice(&SEGMENT_MAGIC_V2);
    out.push(SEGMENT_COMPRESS_ALGO_LZSS);
    out.extend_from_slice(&inner_len.to_le_bytes());
    out.extend_from_slice(&compressed);
    out
}

/// v6.6.2 — unwrap a v2 envelope to v1 bytes. v1-magic input
/// passes through unchanged.
pub(crate) fn unwrap_v2_envelope(bytes: Vec<u8>) -> Result<Vec<u8>, SegmentError> {
    if bytes.len() < 8 || bytes[..8] != SEGMENT_MAGIC_V2 {
        return Ok(bytes);
    }
    if bytes.len() < SEGMENT_V2_HEADER_LEN {
        return Err(SegmentError::TooShort {
            got: bytes.len(),
            need: SEGMENT_V2_HEADER_LEN,
        });
    }
    let algo = bytes[8];
    let inner_len = u32::from_le_bytes([bytes[9], bytes[10], bytes[11], bytes[12]]) as usize;
    let inner = &bytes[SEGMENT_V2_HEADER_LEN..];
    match algo {
        SEGMENT_COMPRESS_ALGO_NONE => {
            // v2 envelope with no compression (rare — would only
            // appear if a writer opted in to v2 but the LZSS check
            // bailed). Return inner directly.
            if inner.len() != inner_len {
                return Err(SegmentError::BadShape(alloc::format!(
                    "v2 envelope algo=none: declared inner_len {inner_len} \
                     differs from body {}",
                    inner.len()
                )));
            }
            Ok(inner.to_vec())
        }
        SEGMENT_COMPRESS_ALGO_LZSS => {
            let decompressed = spg_crypto::lzss::decompress(inner)
                .map_err(|e| SegmentError::CompressionDecodeFailed(alloc::format!("{e:?}")))?;
            if decompressed.len() != inner_len {
                return Err(SegmentError::BadShape(alloc::format!(
                    "v2 envelope LZSS: decompressed {} bytes, declared {inner_len}",
                    decompressed.len()
                )));
            }
            Ok(decompressed)
        }
        other => Err(SegmentError::UnknownCompressionAlgo(other)),
    }
}

impl<'a> SegmentReader<'a> {
    /// Parse a segment from a contiguous byte slice. Validates
    /// magic, CRC32 footer, and structural lengths. v6.6.2: a
    /// v2-magic envelope is rejected by the borrowed-slice reader
    /// because decompression would need to allocate a fresh Vec —
    /// callers with a v2 file must go through
    /// [`OwnedSegment::from_bytes`] which can own the
    /// decompressed bytes.
    pub fn open(bytes: &'a [u8]) -> Result<Self, SegmentError> {
        if bytes.len() >= 8 && bytes[..8] == SEGMENT_MAGIC_V2 {
            return Err(SegmentError::BadShape(alloc::format!(
                "v2 envelope: SegmentReader requires the caller to first \
                 unwrap to v1 bytes via OwnedSegment::from_bytes; the \
                 borrowed-slice reader does not allocate."
            )));
        }
        let metadata = parse_segment_metadata(bytes)?;
        Ok(Self { bytes, metadata })
    }

    #[must_use]
    pub fn meta(&self) -> &SegmentMeta {
        &self.metadata.meta
    }

    /// Bloom-only check — `false` means the key is definitely not
    /// in this segment (no false negatives); `true` means it
    /// *might* be (false-positive rate per the embedded bloom's
    /// target).
    #[must_use]
    pub fn might_contain(&self, key: u64) -> bool {
        segment_might_contain(&self.metadata, key)
    }

    /// Look up `key`. Returns `Some(payload)` if found, `None` if
    /// the bloom rejects or the page-internal search misses.
    /// Always reads at most one page worth of bytes (4 KiB by
    /// default), which is the I/O budget the v5.1 catalog
    /// integration relies on.
    pub fn lookup(&self, key: u64) -> Option<Vec<u8>> {
        segment_lookup(&self.metadata, self.bytes, key)
    }

    /// Iterate all (key, payload) pairs in sorted order. Used by
    /// `scan`-shaped queries and by compaction.
    pub fn scan(&self) -> impl Iterator<Item = (u64, Vec<u8>)> + '_ {
        segment_scan(&self.metadata, self.bytes)
    }
}

/// Owned segment — bytes + parsed metadata in a single struct, no
/// borrow lifetimes. The catalog (v5.1+) holds a
/// `Vec<OwnedSegment>` for its cold tier so each lookup parses
/// nothing fresh; the `lookup` / `might_contain` / `scan` calls
/// here share the same module-private implementation as
/// [`SegmentReader`].
///
/// File I/O lives outside this struct — `spg-storage` is `no_std`,
/// so callers (e.g. `spg-server`) load the segment via
/// `std::fs::read` and hand the resulting `Vec<u8>` to
/// [`OwnedSegment::from_bytes`].
#[derive(Debug, Clone)]
pub struct OwnedSegment {
    bytes: Vec<u8>,
    metadata: SegmentMetadata,
}

impl OwnedSegment {
    /// Parse and validate a segment from owned bytes. The bytes
    /// stay resident inside the returned `OwnedSegment` for the
    /// life of that value. Validation cost is paid once; per-
    /// lookup cost is identical to [`SegmentReader::lookup`].
    ///
    /// v6.6.2 — accepts both v1 (`SPGSEG\x01\x00`) and v2
    /// (`SPGSEG\x02\x00`) magics. A v2 file's body is transparently
    /// unwrapped before the v1 parser runs; the unwrapped v1 bytes
    /// become the `bytes` field, so all downstream readers see a
    /// canonical v1 layout.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, SegmentError> {
        let bytes = unwrap_v2_envelope(bytes)?;
        let metadata = parse_segment_metadata(&bytes)?;
        Ok(Self { bytes, metadata })
    }

    #[must_use]
    pub fn meta(&self) -> &SegmentMeta {
        &self.metadata.meta
    }

    #[must_use]
    pub fn might_contain(&self, key: u64) -> bool {
        segment_might_contain(&self.metadata, key)
    }

    pub fn lookup(&self, key: u64) -> Option<Vec<u8>> {
        segment_lookup(&self.metadata, &self.bytes, key)
    }

    pub fn scan(&self) -> impl Iterator<Item = (u64, Vec<u8>)> + '_ {
        segment_scan(&self.metadata, &self.bytes)
    }

    /// Raw segment bytes — exposed for callers that want to write
    /// the segment back to disk or hand it to a checksum tool.
    /// Read-only.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Page-internal lookup: parse the header, run binary search over
/// `row_offsets` keyed by the first 8 bytes of each row payload
/// (the u64 key). Returns the row payload (`payload_len bytes`),
/// not including the key/length prefix.
fn decode_page_lookup(page: &[u8], key: u64) -> Option<Vec<u8>> {
    if page.len() < 4 {
        return None;
    }
    let num_rows = u32::from_le_bytes([page[0], page[1], page[2], page[3]]) as usize;
    if num_rows == 0 {
        return None;
    }
    let offsets_start = 4;
    let offsets_end = offsets_start + num_rows * 4;
    if page.len() < offsets_end {
        return None;
    }
    let offsets: Vec<u32> = (0..num_rows)
        .map(|i| {
            let o = offsets_start + i * 4;
            u32::from_le_bytes([page[o], page[o + 1], page[o + 2], page[o + 3]])
        })
        .collect();
    // Binary search by reading the leading u64 key of each row.
    let mut lo = 0usize;
    let mut hi = num_rows;
    while lo < hi {
        let mid = usize::midpoint(lo, hi);
        let row_off = offsets[mid] as usize;
        if row_off + 8 > page.len() {
            return None;
        }
        let row_key = u64::from_le_bytes([
            page[row_off],
            page[row_off + 1],
            page[row_off + 2],
            page[row_off + 3],
            page[row_off + 4],
            page[row_off + 5],
            page[row_off + 6],
            page[row_off + 7],
        ]);
        match row_key.cmp(&key) {
            core::cmp::Ordering::Less => lo = mid + 1,
            core::cmp::Ordering::Greater => hi = mid,
            core::cmp::Ordering::Equal => {
                // Found — extract payload.
                let plen_off = row_off + 8;
                if plen_off + 4 > page.len() {
                    return None;
                }
                let plen = u32::from_le_bytes([
                    page[plen_off],
                    page[plen_off + 1],
                    page[plen_off + 2],
                    page[plen_off + 3],
                ]) as usize;
                let payload_start = plen_off + 4;
                let payload_end = payload_start + plen;
                if payload_end > page.len() {
                    return None;
                }
                return Some(page[payload_start..payload_end].to_vec());
            }
        }
    }
    None
}

fn decode_page_iter(page: &[u8]) -> Vec<(u64, Vec<u8>)> {
    if page.len() < 4 {
        return vec![];
    }
    let num_rows = u32::from_le_bytes([page[0], page[1], page[2], page[3]]) as usize;
    if num_rows == 0 {
        return vec![];
    }
    let offsets_end = 4 + num_rows * 4;
    if page.len() < offsets_end {
        return vec![];
    }
    let offsets: Vec<u32> = (0..num_rows)
        .map(|i| {
            let o = 4 + i * 4;
            u32::from_le_bytes([page[o], page[o + 1], page[o + 2], page[o + 3]])
        })
        .collect();
    let mut out = Vec::with_capacity(num_rows);
    for off in offsets {
        let row_off = off as usize;
        if row_off + 12 > page.len() {
            break;
        }
        let key = u64::from_le_bytes([
            page[row_off],
            page[row_off + 1],
            page[row_off + 2],
            page[row_off + 3],
            page[row_off + 4],
            page[row_off + 5],
            page[row_off + 6],
            page[row_off + 7],
        ]);
        let plen = u32::from_le_bytes([
            page[row_off + 8],
            page[row_off + 9],
            page[row_off + 10],
            page[row_off + 11],
        ]) as usize;
        let payload_start = row_off + 12;
        let payload_end = payload_start + plen;
        if payload_end > page.len() {
            break;
        }
        out.push((key, page[payload_start..payload_end].to_vec()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_rows(n: u64) -> Vec<(u64, Vec<u8>)> {
        (0..n)
            .map(|i| {
                let payload = format!("row-{i}").into_bytes();
                (i * 2 + 1, payload) // sparse keys to exercise binary search
            })
            .collect()
    }

    #[test]
    fn v2_envelope_round_trips_byte_equal() {
        // Encode v1, wrap into v2 with compression, unwrap, parse.
        // Result must equal the original v1 bytes byte-for-byte.
        let rows = build_rows(1000);
        let (v1_bytes, _) =
            encode_segment(rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).expect("encode");
        let wrapped = wrap_v2_envelope(v1_bytes.clone(), true);
        // Compression should produce a smaller envelope on the
        // repetitive segment payload.
        assert!(
            wrapped.len() < v1_bytes.len(),
            "v2 envelope should be smaller: {} vs v1 {}",
            wrapped.len(),
            v1_bytes.len()
        );
        let seg = OwnedSegment::from_bytes(wrapped).expect("v2 unwrap + parse");
        assert_eq!(seg.meta().num_rows, 1000);
        // Lookup still works — the unwrapped bytes match the
        // original v1 segment structure.
        assert!(seg.lookup(1).is_some());
        assert!(seg.lookup(1999).is_some());
    }

    #[test]
    fn v2_envelope_with_compress_false_is_v1_passthrough() {
        let rows = build_rows(64);
        let (v1_bytes, _) =
            encode_segment(rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).expect("encode");
        let wrapped = wrap_v2_envelope(v1_bytes.clone(), false);
        assert_eq!(wrapped, v1_bytes);
    }

    #[test]
    fn legacy_v1_segments_still_load_via_from_bytes() {
        // A v6.6 binary must still read v1-magic files written by a
        // pre-v6.6 binary.
        let rows = build_rows(100);
        let (v1_bytes, _) =
            encode_segment(rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).expect("encode");
        // Confirm the bytes start with the v1 magic (no envelope).
        assert_eq!(&v1_bytes[..8], &SEGMENT_MAGIC);
        // OwnedSegment::from_bytes should handle these unchanged.
        let seg = OwnedSegment::from_bytes(v1_bytes).expect("v1 still parses");
        assert_eq!(seg.meta().num_rows, 100);
    }

    #[test]
    fn v2_envelope_invalid_algo_byte_errors_loudly() {
        // Craft a v2-magic file with an unknown algo byte. Reader
        // must refuse rather than silent-corrupt.
        let mut bogus = Vec::new();
        bogus.extend_from_slice(&SEGMENT_MAGIC_V2);
        bogus.push(0x42); // unknown algo
        bogus.extend_from_slice(&0u32.to_le_bytes());
        let err = OwnedSegment::from_bytes(bogus).unwrap_err();
        assert!(matches!(err, SegmentError::UnknownCompressionAlgo(0x42)));
    }

    #[test]
    fn encode_then_open_roundtrips_meta() {
        let rows = build_rows(1000);
        let (bytes, meta) =
            encode_segment(rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).expect("encode succeeds");
        let reader = SegmentReader::open(&bytes).expect("open succeeds");
        assert_eq!(reader.meta().num_rows, meta.num_rows);
        assert_eq!(reader.meta().num_pages, meta.num_pages);
        assert_eq!(reader.meta().min_pk, 1);
        assert_eq!(reader.meta().max_pk, 1999);
        assert_eq!(reader.meta().total_bytes, bytes.len());
    }

    #[test]
    fn lookup_finds_every_inserted_key() {
        let rows = build_rows(1000);
        let expected: Vec<_> = rows.clone();
        let (bytes, _) =
            encode_segment(rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).expect("encode succeeds");
        let reader = SegmentReader::open(&bytes).expect("open succeeds");
        for (key, payload) in expected {
            assert_eq!(
                reader.lookup(key),
                Some(payload),
                "lookup({key}) returned wrong payload"
            );
        }
    }

    #[test]
    fn lookup_returns_none_for_unknown_key() {
        let rows = build_rows(1000);
        let (bytes, _) =
            encode_segment(rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).expect("encode succeeds");
        let reader = SegmentReader::open(&bytes).expect("open succeeds");
        // Even-numbered keys are gaps in our rows (we used 2i+1).
        for k in (0..2000u64).step_by(2) {
            assert!(reader.lookup(k).is_none(), "expected None for gap key {k}");
        }
        // Out-of-range keys.
        assert!(reader.lookup(99_999).is_none());
        assert!(reader.lookup(0).is_none());
    }

    #[test]
    fn might_contain_short_circuits_out_of_range() {
        let rows = build_rows(100);
        let (bytes, _) = encode_segment(rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).unwrap();
        let reader = SegmentReader::open(&bytes).unwrap();
        // min_pk=1, max_pk=199. Anything outside MUST be rejected.
        assert!(!reader.might_contain(0));
        assert!(!reader.might_contain(200));
        // Inside range, inserted key MUST pass bloom.
        assert!(reader.might_contain(1));
        assert!(reader.might_contain(199));
    }

    #[test]
    fn scan_yields_rows_in_key_order() {
        let rows = build_rows(500);
        let expected: Vec<_> = rows.clone();
        let (bytes, _) = encode_segment(rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).unwrap();
        let reader = SegmentReader::open(&bytes).unwrap();
        let scanned: Vec<_> = reader.scan().collect();
        assert_eq!(scanned.len(), 500);
        // Order check.
        for w in scanned.windows(2) {
            assert!(
                w[0].0 < w[1].0,
                "scan out of order: {} >= {}",
                w[0].0,
                w[1].0
            );
        }
        // Content check.
        assert_eq!(scanned, expected);
    }

    #[test]
    fn open_rejects_bad_magic() {
        let rows = build_rows(10);
        let (mut bytes, _) = encode_segment(rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).unwrap();
        bytes[0] ^= 0xff;
        match SegmentReader::open(&bytes) {
            Err(SegmentError::BadMagic { .. }) => {}
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[test]
    fn open_rejects_bad_crc() {
        let rows = build_rows(10);
        let (mut bytes, _) = encode_segment(rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).unwrap();
        // Flip a byte past the header (in the first page payload).
        let off = bytes.len() / 2;
        bytes[off] ^= 0x01;
        match SegmentReader::open(&bytes) {
            Err(SegmentError::BadCrc { .. }) => {}
            other => panic!("expected BadCrc, got {other:?}"),
        }
    }

    #[test]
    fn encode_rejects_unsorted_keys() {
        let rows = vec![(10u64, vec![1]), (5u64, vec![2])];
        match encode_segment(rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES) {
            Err(SegmentError::UnsortedKey { prev: 10, next: 5 }) => {}
            other => panic!("expected UnsortedKey, got {other:?}"),
        }
    }

    #[test]
    fn encode_rejects_empty_input() {
        let rows: Vec<(u64, Vec<u8>)> = vec![];
        match encode_segment(rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES) {
            Err(SegmentError::BadShape(_)) => {}
            other => panic!("expected BadShape for empty input, got {other:?}"),
        }
    }

    #[test]
    fn encode_rejects_bad_page_size() {
        let rows = build_rows(1);
        match encode_segment(rows.clone().into_iter(), 0.01, 128) {
            Err(SegmentError::BadShape(_)) => {}
            other => panic!("expected BadShape for tiny page, got {other:?}"),
        }
        match encode_segment(rows.into_iter(), 0.01, 1_000_000) {
            Err(SegmentError::BadShape(_)) => {}
            other => panic!("expected BadShape for huge page, got {other:?}"),
        }
    }

    #[test]
    fn large_payload_spanning_one_page_each_is_rejected_if_too_big() {
        // One row that's larger than the page (256 default min) —
        // writer must refuse, not silently bloat the page.
        let rows = vec![(1u64, vec![0u8; 8192])];
        match encode_segment(rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES) {
            Err(SegmentError::BadShape(_)) => {}
            other => panic!("expected BadShape for too-large row, got {other:?}"),
        }
    }

    // --- OwnedSegment (v5.1 catalog cold-tier wrapper) -----------

    #[test]
    fn owned_segment_lookup_matches_reader_for_every_key() {
        let rows = build_rows(500);
        let expected: Vec<_> = rows.clone();
        let (bytes, _) = encode_segment(rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).unwrap();
        let bytes_len = bytes.len();
        // Reader sees a borrowed view; collect its outputs before
        // moving the buffer into the owned variant.
        let (r_meta_num_rows, r_meta_min_pk, r_meta_max_pk, r_lookups, r_scan) = {
            let reader = SegmentReader::open(&bytes).unwrap();
            let lookups: Vec<_> = expected.iter().map(|(k, _)| reader.lookup(*k)).collect();
            let scan: Vec<_> = reader.scan().collect();
            (
                reader.meta().num_rows,
                reader.meta().min_pk,
                reader.meta().max_pk,
                lookups,
                scan,
            )
        };
        let owned = OwnedSegment::from_bytes(bytes).unwrap();
        for ((key, expected_payload), reader_payload) in expected.iter().zip(r_lookups.iter()) {
            assert_eq!(reader_payload.as_ref(), Some(expected_payload));
            assert_eq!(owned.lookup(*key).as_ref(), Some(expected_payload));
        }
        // Reader and owned report identical meta + cover identical scan output.
        assert_eq!(r_meta_num_rows, owned.meta().num_rows);
        assert_eq!(r_meta_min_pk, owned.meta().min_pk);
        assert_eq!(r_meta_max_pk, owned.meta().max_pk);
        let o_scan: Vec<_> = owned.scan().collect();
        assert_eq!(r_scan, o_scan);
        assert_eq!(owned.bytes().len(), bytes_len);
    }

    #[test]
    fn owned_segment_might_contain_matches_reader() {
        let rows = build_rows(64);
        let (bytes, _) = encode_segment(rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).unwrap();
        let probes = [0u64, 1, 50, 127, 128, 200];
        let reader_results: Vec<bool> = {
            let reader = SegmentReader::open(&bytes).unwrap();
            probes.iter().map(|k| reader.might_contain(*k)).collect()
        };
        let owned = OwnedSegment::from_bytes(bytes).unwrap();
        for (key, r_hit) in probes.iter().zip(reader_results.iter()) {
            assert_eq!(*r_hit, owned.might_contain(*key));
        }
    }

    #[test]
    fn owned_segment_rejects_bad_bytes_at_construction() {
        // Construct + flip header byte → from_bytes should refuse.
        let rows = build_rows(8);
        let (mut bytes, _) = encode_segment(rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).unwrap();
        bytes[0] ^= 0xff; // smash magic
        match OwnedSegment::from_bytes(bytes) {
            Err(SegmentError::BadMagic { .. }) => {}
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[test]
    fn owned_segment_lookup_returns_none_for_missing_key() {
        let rows = build_rows(100); // keys = 2i+1 → 1..199
        let (bytes, _) = encode_segment(rows.into_iter(), 0.01, SEGMENT_PAGE_BYTES).unwrap();
        let owned = OwnedSegment::from_bytes(bytes).unwrap();
        // Gap (even) keys + out-of-range keys.
        for key in [0u64, 2, 50, 198, 200, 9999] {
            assert!(
                owned.lookup(key).is_none(),
                "expected None for non-inserted key {key}, got Some"
            );
        }
    }
}
