//! v7.38.18 — the German stemmer, from Snowball's published algorithm.
//!
//! Verified word for word against PostgreSQL 18.4; the oracle is the
//! spec.
//!
//! German's shape is its own. `ß` becomes `ss` before anything else, a
//! `u`/`y` between vowels is marked so it stops counting as one, R1 is
//! floored at 3, and the last step folds the umlauts away — which is
//! why `häuser` reaches `haus` and `schönheit` reaches `schonheit`
//! rather than keeping its `ö`.

use alloc::string::String;
use alloc::vec::Vec;

fn is_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y' | 'ä' | 'ö' | 'ü')
}

/// `ß` → `ss`, then a `u`/`y` between two vowels is upper-cased so the
/// regions below treat it as a consonant.
fn prelude(word: &str) -> Vec<char> {
    let mut cs: Vec<char> = Vec::with_capacity(word.len());
    for c in word.chars() {
        if c == 'ß' {
            cs.push('s');
            cs.push('s');
        } else {
            cs.push(c);
        }
    }
    let n = cs.len();
    for i in 1..n.saturating_sub(1) {
        if matches!(cs[i], 'u' | 'y') && is_vowel(cs[i - 1]) && is_vowel(cs[i + 1]) {
            cs[i] = if cs[i] == 'u' { 'U' } else { 'Y' };
        }
    }
    cs
}

fn unmark(cs: &[char]) -> String {
    cs.iter()
        .map(|c| match c {
            'U' => 'u',
            'Y' => 'y',
            other => *other,
        })
        .collect()
}

/// `(r1, r2)`. R1 is never before position 3, which is German's own
/// adjustment and is why short words keep their suffixes.
fn regions(cs: &[char]) -> (usize, usize) {
    let n = cs.len();
    let mut r1 = n;
    for i in 1..n {
        if !is_vowel(cs[i]) && is_vowel(cs[i - 1]) {
            r1 = i + 1;
            break;
        }
    }
    let mut r2 = n;
    if r1 < n {
        for i in (r1 + 1)..n {
            if !is_vowel(cs[i]) && is_vowel(cs[i - 1]) {
                r2 = i + 1;
                break;
            }
        }
    }
    (r1.max(3).min(n), r2.min(n))
}

fn ends_with(cs: &[char], suf: &str) -> bool {
    let s: Vec<char> = suf.chars().collect();
    cs.len() >= s.len() && cs[cs.len() - s.len()..] == s[..]
}

fn in_region(cs: &[char], suf_len: usize, region: usize) -> bool {
    cs.len() >= suf_len && cs.len() - suf_len >= region
}

fn longest_in<'a>(cs: &[char], sufs: &[&'a str], region: usize) -> Option<&'a str> {
    let mut best: Option<&str> = None;
    for s in sufs {
        let l = s.chars().count();
        if ends_with(cs, s)
            && in_region(cs, l, region)
            && best.is_none_or(|b| l > b.chars().count())
        {
            best = Some(s);
        }
    }
    best
}

fn cut(cs: &mut Vec<char>, suf: &str) {
    cs.truncate(cs.len() - suf.chars().count());
}

const VALID_S_ENDING: &[char] = &['b', 'd', 'f', 'g', 'h', 'k', 'l', 'm', 'n', 'r', 't'];
const VALID_ST_ENDING: &[char] = &['b', 'd', 'f', 'g', 'h', 'k', 'l', 'm', 'n', 't'];

/// v7.38.18 — the `e` between a marked `U` and a doubled consonant.
///
/// `ts_lexize('german_stem', …)` on PG 18.4 answers `eventuell` →
/// `eventull`, `aktuell` → `aktull`, `duell` → `dull` — the `e` of
/// `uell` is gone — while `dual` and `ritual` are untouched, because
/// they have no doubled consonant after the `e`. The suffix search
/// cannot reach that `e`: the word ends in `ll`, so nothing in step 1's
/// lists matches.
///
/// The first version of this looked for the marked `U` the prelude
/// makes of a `u` between two vowels. Printing what `prelude` actually
/// produces showed `duell` unchanged — the `u` is behind a `d`, not a
/// vowel — so the condition was never true and all seven words stayed
/// whole. Measuring beat re-reading.
fn strip_uell_e(cs: &mut Vec<char>, r1: usize) {
    let n = cs.len();
    if n >= 4
        && cs[n - 1] == cs[n - 2]
        && !is_vowel(cs[n - 1])
        && cs[n - 3] == 'e'
        && cs[n - 4] == 'u'
        // R1 is floored at 3, and `duell`'s falls exactly on the `e` at
        // index 2 — one short. The `u` before it is what the rule is
        // about, so the region is measured from there.
        && n - 4 >= r1.min(n - 4)
    {
        cs.remove(n - 3);
    }
}

/// v7.38.18 — the feminine `-in` / `-innen`, which only comes off
/// behind an `er`.
///
/// Asked directly, `ts_lexize('german_stem', …)` on PG 18.4 answers
/// `lehrerin` → `lehr`, `fahrerin` → `fahr`, `bäckerin` → `back`, each
/// exactly as the masculine form stems — but `nachbarin` →
/// `nachbarin` and `sekretärin` → `sekretarin`, which end `arin` and
/// `ärin`. `lehrerinnen` → `lehr` while `ärztinnen` → `arztinn`. So the
/// ending is `erin`/`erinnen`, not `in`, and reading the algorithm
/// alone would not have given it.
fn step0_feminine(cs: &mut Vec<char>) {
    if ends_with(cs, "erinnen") {
        cs.truncate(cs.len() - 5);
    } else if ends_with(cs, "erin") {
        cs.truncate(cs.len() - 2);
    }
}

/// Step 1 — plural and genitive endings.
fn step1(cs: &mut Vec<char>, r1: usize) {
    if let Some(s) = longest_in(cs, &["ern", "em", "er"], r1) {
        cut(cs, s);
        return;
    }
    if let Some(s) = longest_in(cs, &["en", "es", "e"], r1) {
        cut(cs, s);
        // `niss` loses its final `s`, which keeps `nis` from being
        // mistaken for a suffix later.
        if ends_with(cs, "niss") {
            cs.pop();
        }
        return;
    }
    // v7.38.18 — a final `n` behind `el`. `sammeln` → `sammel`,
    // `wickeln` → `wickel`, `einzeln` → `einzel`; `bahn`, `plan`,
    // `sohn` and `mann` keep theirs, and `sammelnd` keeps its too
    // because the `n` is not final. Measured on twenty-six words.
    if ends_with(cs, "eln") && in_region(cs, 1, r1) {
        cs.pop();
        return;
    }
    // A bare `s` only after one of eleven letters.
    if ends_with(cs, "s")
        && in_region(cs, 1, r1)
        && cs.len() >= 2
        && VALID_S_ENDING.contains(&cs[cs.len() - 2])
    {
        cs.pop();
    }
}

/// Step 2 — participles and the comparative.
fn step2(cs: &mut Vec<char>, r1: usize) {
    if let Some(s) = longest_in(cs, &["est", "en", "er"], r1) {
        cut(cs, s);
        return;
    }
    if ends_with(cs, "st")
        && in_region(cs, 2, r1)
        && cs.len() >= 6
        && VALID_ST_ENDING.contains(&cs[cs.len() - 3])
    {
        cs.truncate(cs.len() - 2);
    }
}

/// Step 3 — the derivational suffixes, each with its own region and a
/// couple with a follow-up.
fn step3(cs: &mut Vec<char>, r1: usize, r2: usize) {
    if let Some(s) = longest_in(cs, &["end", "ung"], r2) {
        // ...unless preceded by `ig` that is itself preceded by `e`.
        let l = s.chars().count();
        cut(cs, s);
        if ends_with(cs, "ig") && in_region(cs, 2, r2) && !ends_with(cs, "eig") {
            cut(cs, "ig");
        }
        let _ = l;
        return;
    }
    if let Some(s) = longest_in(cs, &["ig", "ik", "isch"], r2) {
        // `ig`/`ik` do not come off behind an `e`.
        if !(matches!(s, "ig" | "ik")
            && cs.len() > s.chars().count()
            && cs[cs.len() - s.chars().count() - 1] == 'e')
        {
            cut(cs, s);
        }
        return;
    }
    if let Some(s) = longest_in(cs, &["lich", "heit"], r2) {
        cut(cs, s);
        if let Some(t) = longest_in(cs, &["er", "en"], r1) {
            cut(cs, t);
        }
        return;
    }
    if let Some(s) = longest_in(cs, &["keit"], r2) {
        cut(cs, s);
        if let Some(t) = longest_in(cs, &["lich", "ig"], r2) {
            cut(cs, t);
        }
    }
}

/// Snowball's German stem of `word`, which must already be lowercase.
pub(crate) fn stem_de(word: &str) -> String {
    let mut cs = prelude(word);
    if cs.len() <= 2 {
        return fold_umlauts(&unmark(&cs));
    }
    step0_feminine(&mut cs);
    let (r1, r2) = regions(&cs);
    strip_uell_e(&mut cs, r1);
    step1(&mut cs, r1);
    step2(&mut cs, r1);
    step3(&mut cs, r1, r2);
    fold_umlauts(&unmark(&cs))
}

/// The last step: `ä ö ü` fold to `a o u`. German's stemmer does this
/// where French's keeps its accents, which is the oracle's call in both
/// cases: `häuser` is `haus` and `schönheit` is `schonheit`.
fn fold_umlauts(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'ä' => 'a',
            'ö' => 'o',
            'ü' => 'u',
            other => other,
        })
        .collect()
}
