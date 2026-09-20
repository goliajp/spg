//! v7.15.0 — trigram extraction for `pg_trgm`-compatible GIN
//! indexes, and the `pg_trgm` similarity functions themselves.
//!
//! PG's recipe, established by differential measurement against a
//! live PG 18.6 (`show_trgm`, `similarity`, `word_similarity` and
//! `strict_word_similarity` over 248 inputs including Greek,
//! Cyrillic, Korean, Japanese, an emoji and `ß`):
//!
//!   1. Lowercase the input.
//!   2. Split into words at every character that is not
//!      alphanumeric. Alphanumeric is the Unicode property, not
//!      the ASCII one: `日本語` is one word, and an apostrophe is
//!      a separator (`don't` is the two words `don` and `t`).
//!   3. For each word: pad as `"  word "` (two leading spaces,
//!      one trailing space) and emit every overlapping 3-CHARACTER
//!      window.
//!   4. Compact each window to three bytes. A window of three
//!      ASCII characters is already three bytes and keeps them.
//!      Anything wider is hashed: PG's legacy CRC-32 over the
//!      window's UTF-8 bytes, of which the low three bytes in
//!      little-endian memory order become the trigram. That hash
//!      is why `show_trgm('日本語')` prints `0x`-prefixed numbers
//!      rather than text, and why two distinct wide trigrams can
//!      collide — PG conflates them too, so matching its hash is
//!      what makes `similarity()` agree on non-ASCII input.
//!
//! The padding is what makes `LIKE 'foo%'` match the trigrams
//! `"  f"`, `" fo"`, `"foo"` — the same trigrams a stored value
//! starting with `foo` would emit, so the index can produce
//! candidates for left-anchored matches.

extern crate alloc;

use alloc::borrow::Cow;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// r1019 — a trigram, as three bytes rather than a `String`.
///
/// It was a `String`, and `extract_trigrams` allocated one per WINDOW —
/// before deduplication, so a 3.4 KB message body cost ~3,400 heap
/// allocations per trigram index, four times over on mailrs's schema.
/// Measured: their four `gin_trgm_ops` indexes were 9.2 GB of the 16.7 GB
/// this import allocates in total, 797 MB of its 1,382 MB live high-water,
/// and 1,044 MB of its 2,193 MB peak RSS. Three bytes in a 24-byte header
/// plus a heap block, hundreds of millions of times.
///
/// Three bytes is also exactly what PG stores: a window of three ASCII
/// characters keeps its bytes, and a wider one is hashed down to three
/// (see the module header). Ordering is bytewise, as PG's is.
pub type Trigram = [u8; 3];

/// A trigram is rendered as text when all three bytes are ASCII
/// alphanumerics or spaces, and as `0x` + six hex digits otherwise. That is
/// PG's rule for `show_trgm`, and the same rendering keys the `String`-keyed
/// posting map, so a hashed trigram addresses the map under a name no
/// three-character trigram can also spell.
#[must_use]
pub fn trigram_key(t: &Trigram) -> Cow<'_, str> {
    if t.iter().all(|&b| b == b' ' || b.is_ascii_alphanumeric()) {
        // Every byte is printable ASCII, hence valid UTF-8.
        Cow::Borrowed(core::str::from_utf8(t).expect("all three bytes are ASCII"))
    } else {
        Cow::Owned(format!(
            "0x{:06x}",
            (u32::from(t[0]) << 16) | (u32::from(t[1]) << 8) | u32::from(t[2])
        ))
    }
}

/// PG orders a trigram array by comparing its three bytes as SIGNED
/// chars, so a hashed trigram whose first byte is `0xad` sorts BEFORE
/// `"  c"`. `show_trgm`'s output order is where that shows.
#[must_use]
#[allow(clippy::cast_possible_wrap)]
pub const fn signed_order(t: &Trigram) -> [i8; 3] {
    [t[0] as i8, t[1] as i8, t[2] as i8]
}

/// PG's legacy CRC-32: the reflected (zlib) table driven by the
/// most-significant-byte-first loop. The combination is not any standard
/// CRC-32 — it is PG's own, and `show_trgm` on non-ASCII input is the
/// observable that pins it.
const LEGACY_CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

fn legacy_crc32(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in bytes {
        let idx = (((crc >> 24) as u8) ^ b) as usize;
        crc = LEGACY_CRC32_TABLE[idx] ^ (crc << 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// Three characters down to three bytes. Three ASCII characters are already
/// three bytes; anything wider is hashed, and the three bytes taken are the
/// CRC's low three in little-endian order.
fn compact(w: [char; 3]) -> Trigram {
    let mut buf = [0u8; 12];
    let mut n = 0usize;
    for ch in w {
        n += ch.encode_utf8(&mut buf[n..]).len();
    }
    if n == 3 {
        [buf[0], buf[1], buf[2]]
    } else {
        let crc = legacy_crc32(&buf[..n]);
        [
            (crc & 0xFF) as u8,
            ((crc >> 8) & 0xFF) as u8,
            ((crc >> 16) & 0xFF) as u8,
        ]
    }
}

/// Emit every trigram of one word, in order. `word` is already lowercased
/// and holds no separators; the padding is added here.
fn word_trigrams<F: FnMut(Trigram)>(word: &[char], mut emit: F) {
    let mut padded: Vec<char> = Vec::with_capacity(word.len() + 3);
    padded.push(' ');
    padded.push(' ');
    padded.extend_from_slice(word);
    padded.push(' ');
    for w in padded.windows(3) {
        emit(compact([w[0], w[1], w[2]]));
    }
}

/// Extract the set of trigrams (lowercased, space-padded) from
/// the input. Returns a deduplicated, sorted `BTreeSet`.
///
/// Matches PG `show_trgm`.
pub fn extract_trigrams(input: &str) -> BTreeSet<Trigram> {
    let mut out = BTreeSet::new();
    for word in split_into_words(input) {
        word_trigrams(&word, |t| {
            out.insert(t);
        });
    }
    out
}

/// The trigrams of `input` in reading order, plus the `[start, end)` range
/// each word occupies in that list. `word_similarity` needs the order and
/// the word boundaries; `extract_trigrams` needs neither.
fn ordered_trigrams(input: &str) -> (Vec<Trigram>, Vec<(usize, usize)>) {
    let mut list = Vec::new();
    let mut bounds = Vec::new();
    for word in split_into_words(input) {
        let start = list.len();
        word_trigrams(&word, |t| list.push(t));
        bounds.push((start, list.len()));
    }
    (list, bounds)
}

/// Lower-bound count of trigrams in the input. Used by
/// `similarity()` without materialising the full set when only
/// the cardinality matters.
pub fn trigram_count(input: &str) -> usize {
    extract_trigrams(input).len()
}

/// PG `similarity(a, b)` — Jaccard ratio of trigram sets:
/// `|trgm(a) ∩ trgm(b)| / |trgm(a) ∪ trgm(b)|`. Returns a value
/// in `[0.0, 1.0]`; `0.0` when both sides have no trigrams.
pub fn similarity(a: &str, b: &str) -> f64 {
    let sa = extract_trigrams(a);
    let sb = extract_trigrams(b);
    if sa.is_empty() && sb.is_empty() {
        return 0.0;
    }
    let inter = sa.intersection(&sb).count();
    let union = sa.len() + sb.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// PG `word_similarity(a, b)` — the greatest similarity between `a`'s
/// trigram set and any contiguous extent of `b`'s ordered trigram list.
/// The extent's score is the same ratio `similarity` uses, counted over
/// the extent rather than over all of `b`.
pub fn word_similarity(a: &str, b: &str) -> f64 {
    best_extent_similarity(a, b, false)
}

/// PG `strict_word_similarity(a, b)` — `word_similarity` restricted to
/// extents that cover whole words of `b`.
pub fn strict_word_similarity(a: &str, b: &str) -> f64 {
    best_extent_similarity(a, b, true)
}

fn best_extent_similarity(a: &str, b: &str, strict: bool) -> f64 {
    let set_a = extract_trigrams(a);
    let len1 = set_a.len();
    let (list, bounds) = ordered_trigrams(b);
    if len1 == 0 || list.is_empty() {
        return 0.0;
    }

    // Dense ids, so the sliding window counts with a flat `Vec` instead of
    // a map lookup per step.
    let mut ids: BTreeMap<Trigram, usize> = BTreeMap::new();
    let mut in_a: Vec<bool> = Vec::new();
    let mut id_of: Vec<usize> = Vec::with_capacity(list.len());
    for t in &list {
        let next = ids.len();
        let id = *ids.entry(*t).or_insert(next);
        if id == in_a.len() {
            in_a.push(set_a.contains(t));
        }
        id_of.push(id);
    }
    // A word with no trigram of `a` in it can never begin or end the best
    // extent: dropping it leaves the matched count alone and can only
    // shrink the distinct count.
    let matches: Vec<bool> = id_of.iter().map(|&id| in_a[id]).collect();
    let word_matches: Vec<bool> = bounds
        .iter()
        .map(|&(s, e)| matches[s..e].iter().any(|m| *m))
        .collect();

    let starts: Vec<usize> = if strict {
        bounds
            .iter()
            .zip(&word_matches)
            .filter(|(_, m)| **m)
            .map(|(&(s, _), _)| s)
            .collect()
    } else {
        (0..list.len()).filter(|&i| matches[i]).collect()
    };
    // Where an extent may end. The strict variant ends only where a word
    // ends — and the word's LAST trigram need not be one of `a`'s, which
    // is why this is a separate list rather than a reading of `matches`.
    let mut ends_here: Vec<bool> = alloc::vec![false; list.len()];
    if strict {
        for (&(_, e), m) in bounds.iter().zip(&word_matches) {
            if *m {
                ends_here[e - 1] = true;
            }
        }
    } else {
        ends_here.copy_from_slice(&matches);
    }

    let mut cnt: Vec<u32> = alloc::vec![0; ids.len()];
    let mut touched: Vec<usize> = Vec::new();
    let mut best = 0.0f64;
    for &s in &starts {
        let (mut distinct, mut matched) = (0usize, 0usize);
        for j in s..list.len() {
            let id = id_of[j];
            if cnt[id] == 0 {
                distinct += 1;
                if in_a[id] {
                    matched += 1;
                }
                touched.push(id);
            }
            cnt[id] += 1;
            if ends_here[j] {
                let denom = len1 + distinct - matched;
                let smlr = matched as f64 / denom as f64;
                if smlr > best {
                    best = smlr;
                }
            }
            // `matched` is capped by `len1`, so once the extent holds more
            // than `len1 / best` distinct trigrams no longer extent from
            // this start can beat what is already in hand.
            if best > 0.0 && (distinct as f64) > len1 as f64 / best {
                break;
            }
        }
        for id in touched.drain(..) {
            cnt[id] = 0;
        }
    }
    best
}

/// Extract trigrams that a LIKE pattern's non-wildcard substrings
/// guarantee must appear in every matching row. Wildcards (`%`,
/// `_`) split the pattern into literal runs, and a run splits further at
/// every non-alphanumeric character, because the index holds trigrams of
/// words and a word stops there too. Backslash escapes the next char (so
/// `\%` and `\_` are literal). Anchoring matters: a sub-word that begins
/// its word (the pattern is anchored there, or a separator precedes it)
/// contributes its left-padded trigrams (`"  f"`, `" fo"`, …); one that
/// ends its word contributes the right-padded one.
///
/// Returns `None` when the pattern carries no usable constraint, which
/// signals the caller to fall back to a full scan.
pub fn trigrams_from_like_pattern(pattern: &str) -> Option<BTreeSet<Trigram>> {
    // Split on wildcards, honoring backslash escapes.
    let mut runs: Vec<(
        String,
        bool, /* leading-anchored */
        bool, /* trailing-anchored */
    )> = Vec::new();
    let mut cur = String::new();
    let mut iter = pattern.chars();
    let mut leading = true;
    while let Some(c) = iter.next() {
        match c {
            '\\' => {
                if let Some(next) = iter.next() {
                    cur.push(next);
                }
            }
            '%' | '_' => {
                if !cur.is_empty() {
                    runs.push((core::mem::take(&mut cur), leading, false));
                }
                leading = false;
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        runs.push((cur, leading, true));
    }
    let mut out = BTreeSet::new();
    for (run, anchored_left, anchored_right) in &runs {
        let chars: Vec<char> = run.chars().collect();
        let mut i = 0usize;
        while i < chars.len() {
            if !chars[i].is_alphanumeric() {
                i += 1;
                continue;
            }
            let start = i;
            let mut word: Vec<char> = Vec::new();
            while i < chars.len() && chars[i].is_alphanumeric() {
                for lc in chars[i].to_lowercase() {
                    word.push(lc);
                }
                i += 1;
            }
            // A separator inside the run proves the word starts (or ends)
            // there just as firmly as the pattern's own anchor does.
            let pad_left = start > 0 || *anchored_left;
            let pad_right = i < chars.len() || *anchored_right;
            let mut padded: Vec<char> = Vec::with_capacity(word.len() + 3);
            if pad_left {
                padded.push(' ');
                padded.push(' ');
            }
            padded.extend_from_slice(&word);
            if pad_right {
                padded.push(' ');
            }
            for w in padded.windows(3) {
                out.insert(compact([w[0], w[1], w[2]]));
            }
        }
    }
    if out.is_empty() {
        // No usable constraint — caller falls back.
        None
    } else {
        Some(out)
    }
}

/// The input's words, lowercased, in reading order. A word is a maximal run
/// of alphanumeric characters — the Unicode property, so `日本語` is one
/// word and `don't` is two.
fn split_into_words(input: &str) -> Vec<Vec<char>> {
    let mut out: Vec<Vec<char>> = Vec::new();
    let mut cur: Vec<char> = Vec::new();
    for c in input.chars() {
        if c.is_alphanumeric() {
            for lc in c.to_lowercase() {
                cur.push(lc);
            }
        } else if !cur.is_empty() {
            out.push(core::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    fn collect(input: &str) -> Vec<String> {
        extract_trigrams(input)
            .iter()
            .map(|t| trigram_key(t).to_string())
            .collect()
    }

    #[test]
    fn padding_and_lowercase() {
        // PG: `show_trgm('hi')` → `{"  h", " hi", "hi "}`
        let trs = collect("Hi");
        assert_eq!(trs, vec!["  h", " hi", "hi "]);
    }

    #[test]
    fn multi_word_splits_on_space() {
        let trs = collect("foo bar");
        // Two words, each padded; trigrams from each. Sets union
        // so each unique trigram appears once.
        assert!(trs.contains(&"foo".to_string()));
        assert!(trs.contains(&"bar".to_string()));
        // No cross-word trigram like "o b".
        assert!(!trs.contains(&"o b".to_string()));
    }

    #[test]
    fn apostrophe_is_a_separator() {
        // PG 18.6: `show_trgm('don''t')` → `{"  d","  t"," do"," t ",don,"on "}`.
        let mut trs = collect("don't");
        trs.sort();
        assert_eq!(trs, vec!["  d", "  t", " do", " t ", "don", "on "]);
    }

    #[test]
    fn wide_characters_are_hashed_the_way_pg_hashes_them() {
        // PG 18.6: `show_trgm('日本語')` →
        // `{0x8194c0,0x836e53,0x1dc363,0x1e22e9}`.
        let mut trs = collect("日本語");
        trs.sort();
        assert_eq!(trs, vec!["0x1dc363", "0x1e22e9", "0x8194c0", "0x836e53"]);
    }

    #[test]
    fn similarity_identical() {
        assert!((similarity("hello", "hello") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn similarity_disjoint() {
        assert!(similarity("foo", "xyz") < 0.1);
    }

    #[test]
    fn similarity_reaches_wide_characters() {
        // PG 18.6: `similarity('日本語', '日本')` → 0.4. Before the wide
        // windows were hashed in, every CJK string had an empty trigram
        // set and this was 0.
        assert!((similarity("日本語", "日本") - 0.4).abs() < 1e-6);
    }

    #[test]
    fn word_similarity_scores_the_best_extent() {
        // PG 18.6: word_similarity('word','two words') → 0.8,
        // strict_word_similarity(…) → 0.5714286, similarity(…) → 0.36363637.
        assert!((word_similarity("word", "two words") - 0.8).abs() < 1e-6);
        assert!((strict_word_similarity("word", "two words") - 4.0 / 7.0).abs() < 1e-6);
        assert!((similarity("word", "two words") - 4.0 / 11.0).abs() < 1e-6);
    }

    #[test]
    fn word_similarity_pays_for_a_gap_inside_the_extent() {
        // PG 18.6: word_similarity('abcd','ab xcd') → 0.4. Counting only
        // the matched trigrams over `len1` would say 0.6 — the extent's own
        // distinct count is in the denominator.
        assert!((word_similarity("abcd", "ab xcd") - 0.4).abs() < 1e-6);
        assert!((strict_word_similarity("abcd", "ab xcd") - 1.0 / 3.0).abs() < 1e-6);
    }

    #[test]
    fn word_similarity_without_a_match_is_zero() {
        assert!(word_similarity("ab", "xaybzb") < 1e-9);
        assert!(word_similarity("", "abc") < 1e-9);
        assert!(word_similarity("abc", "") < 1e-9);
    }

    #[test]
    fn like_pattern_anchored() {
        // `LIKE 'foo%'` → leading-anchored literal "foo", which
        // pads as "  foo".
        let trs = trigrams_from_like_pattern("foo%").unwrap();
        assert!(trs.contains(b"  f"));
        assert!(trs.contains(b" fo"));
        assert!(trs.contains(b"foo"));
    }

    #[test]
    fn like_pattern_unanchored_short_run() {
        // `LIKE '%foo%'` → unanchored literal "foo". Only
        // internal trigram "foo" applies; no padding.
        let trs = trigrams_from_like_pattern("%foo%").unwrap();
        assert!(trs.contains(b"foo"));
        assert!(!trs.contains(b"  f"));
    }

    #[test]
    fn like_pattern_no_usable_constraint() {
        // No literal run of length ≥ 3 unanchored, AND no
        // anchored runs at all → still produces nothing usable
        // for plain `%ab%`. Caller falls back.
        assert!(trigrams_from_like_pattern("%ab%").is_none());
        assert!(trigrams_from_like_pattern("%%").is_none());
    }

    #[test]
    fn like_pattern_splits_at_a_separator() {
        // `LIKE '%foo-bar%'` must not demand `oo-`, `o-b` or `-ba`: a row
        // holding `foo-bar` is indexed as the two words `foo` and `bar`,
        // so demanding a trigram that spans the dash drops every match.
        let trs = trigrams_from_like_pattern("%foo-bar%").unwrap();
        let keys: Vec<String> = trs.iter().map(|t| trigram_key(t).to_string()).collect();
        let want: Vec<String> = ["foo", "oo ", "  b", " ba", "bar"]
            .iter()
            .map(|s| String::from(*s))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        assert_eq!(keys, want);
    }

    #[test]
    fn like_pattern_escaped_wildcard_is_still_a_separator() {
        // `LIKE '\%foo'` → the literal run is `%foo`. `%` is not a word
        // character, so the indexed word is `foo` and the run demands
        // `"  f"`, not `"  %"` — which no indexed row could ever hold.
        let trs = trigrams_from_like_pattern("\\%foo").unwrap();
        assert!(trs.contains(b"  f"));
        assert!(!trs.contains(b"  %"));
    }
}
