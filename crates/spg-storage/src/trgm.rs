//! v7.15.0 — trigram extraction for `pg_trgm`-compatible GIN
//! indexes. Mirrors PG's `pg_trgm` extension closely enough that
//! `gin_trgm_ops` indexes built on the same source produce the
//! same trigram set and `similarity(a, b)` returns the same
//! Jaccard ratio.
//!
//! PG's recipe (paraphrased from `contrib/pg_trgm/trgm_op.c`):
//!   1. Lowercase the input.
//!   2. Split on non-alphanumeric runs into words; each maximal
//!      `[a-zA-Z0-9]+` run is a word. Apostrophe (`'`) is also
//!      treated as a word character.
//!   3. For each word: pad as `"  word "` (two leading spaces,
//!      one trailing space) and emit every overlapping 3-byte
//!      window.
//!
//! The padding is what makes `LIKE 'foo%'` match the trigrams
//! `"  f"`, `" fo"`, `"foo"` — the same trigrams a stored value
//! starting with `foo` would emit, so the index can produce
//! candidates for left-anchored matches.
//!
//! Non-ASCII characters are passed through unchanged. PG's
//! pg_trgm treats them as opaque alphanumerics when MULTIBYTE is
//! enabled, which lines up with this module's "everything not in
//! `[A-Za-z0-9']` is a separator" rule (a UTF-8 multibyte
//! continuation byte has the high bit set and so isn't `'` or
//! ASCII alphanumeric — without explicit support it would split
//! mid-codepoint). SPG accepts the simplification: trigrams from
//! mixed-script columns may under-cover relative to PG, but the
//! re-evaluation pass at query time still gives correct LIKE
//! semantics on every candidate.

extern crate alloc;

use alloc::collections::BTreeSet;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Extract the set of trigrams (lowercased, space-padded) from
/// the input. Returns a deduplicated, sorted `BTreeSet`.
///
/// Matches PG `show_trgm` for ASCII inputs.
pub fn extract_trigrams(input: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for word in split_into_words(input) {
        let padded = pad_word(&word);
        let bytes = padded.as_bytes();
        if bytes.len() < 3 {
            continue;
        }
        for i in 0..=bytes.len() - 3 {
            // bytes already pass through `pad_word`, which keeps
            // only ASCII alphanumerics (and the apostrophe);
            // pre-validated UTF-8.
            if let Ok(s) = core::str::from_utf8(&bytes[i..i + 3]) {
                out.insert(s.to_string());
            }
        }
    }
    out
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

/// Extract trigrams that a LIKE pattern's non-wildcard substrings
/// guarantee must appear in every matching row. Wildcards (`%`,
/// `_`) split the pattern into literal runs; only runs of ≥ 3
/// chars contribute trigrams (a 1- or 2-char run constrains
/// nothing on its own). Backslash escapes the next char (so
/// `\%` and `\_` are literal). Anchoring matters: a literal run
/// at the start contributes its left-padded trigrams (`"  f"`,
/// ` fo`, …); a run in the middle contributes only the
/// run-internal trigrams; a trailing run contributes its
/// right-padded trigrams.
///
/// Returns `None` when the pattern carries no usable constraint
/// (no literal run of length ≥ 1 after wildcard analysis), which
/// signals the caller to fall back to a full scan.
pub fn trigrams_from_like_pattern(pattern: &str) -> Option<BTreeSet<String>> {
    // Split on wildcards, honoring backslash escapes.
    let mut runs: Vec<(String, bool /* leading-anchored */, bool /* trailing-anchored */)> =
        Vec::new();
    let mut cur = String::new();
    let mut iter = pattern.chars().peekable();
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
        // Build a padded form per anchoring. For an unanchored
        // run, only the internal trigrams of the run apply; for
        // a left-anchored run, pad `"  run"`; for a
        // right-anchored, pad `"run "`. Both → `"  run "`.
        let mut padded = String::new();
        if *anchored_left {
            padded.push_str("  ");
        }
        padded.push_str(&run.to_lowercase());
        if *anchored_right {
            padded.push(' ');
        }
        let bytes = padded.as_bytes();
        if bytes.len() < 3 {
            continue;
        }
        for i in 0..=bytes.len() - 3 {
            if let Ok(s) = core::str::from_utf8(&bytes[i..i + 3]) {
                out.insert(s.to_string());
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

fn split_into_words(input: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for c in input.chars() {
        if is_word_char(c) {
            // Lowercase ASCII; non-ASCII passes through.
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

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '\''
}

fn pad_word(word: &str) -> String {
    let mut s = String::with_capacity(word.len() + 3);
    s.push_str("  ");
    s.push_str(word);
    s.push(' ');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn collect(input: &str) -> Vec<String> {
        extract_trigrams(input).into_iter().collect()
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
    fn similarity_identical() {
        assert!((similarity("hello", "hello") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn similarity_disjoint() {
        assert!(similarity("foo", "xyz") < 0.1);
    }

    #[test]
    fn like_pattern_anchored() {
        // `LIKE 'foo%'` → leading-anchored literal "foo", which
        // pads as "  foo".
        let trs = trigrams_from_like_pattern("foo%").unwrap();
        assert!(trs.contains("  f"));
        assert!(trs.contains(" fo"));
        assert!(trs.contains("foo"));
    }

    #[test]
    fn like_pattern_unanchored_short_run() {
        // `LIKE '%foo%'` → unanchored literal "foo". Only
        // internal trigram "foo" applies; no padding.
        let trs = trigrams_from_like_pattern("%foo%").unwrap();
        assert!(trs.contains("foo"));
        assert!(!trs.contains("  f"));
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
    fn like_pattern_backslash_escapes() {
        // `LIKE '\%foo'` → leading-anchored literal "%foo".
        let trs = trigrams_from_like_pattern("\\%foo").unwrap();
        // First trigram pads with two leading spaces then '%'.
        assert!(trs.contains("  %"));
    }
}
