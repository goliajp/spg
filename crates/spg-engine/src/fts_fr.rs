//! v7.38.18 — the French stemmer, from Snowball's published algorithm.
//!
//! Verified word for word against PostgreSQL 18.4; the oracle is the
//! spec and nothing here is transcribed from anyone's source.
//!
//! French has two steps Spanish does not. Before anything else, a `u`
//! or `i` BETWEEN vowels and a `y` next to one are marked so they stop
//! counting as vowels — otherwise `payer` and `paie` compute different
//! regions. And after the suffixes come off there is an unelision step
//! that removes a final `ë`/`é` and undoes a doubled consonant, which
//! is why `appelle` reaches `appel` rather than `appell`.

use alloc::string::String;
use alloc::vec::Vec;

fn is_vowel(c: char) -> bool {
    matches!(
        c,
        'a' | 'e'
            | 'i'
            | 'o'
            | 'u'
            | 'y'
            | 'â'
            | 'à'
            | 'ë'
            | 'é'
            | 'ê'
            | 'è'
            | 'ï'
            | 'î'
            | 'ô'
            | 'û'
            | 'ù'
    )
}

/// Snowball marks a non-syllabic `u`/`i`/`y` by upper-casing it, so the
/// region arithmetic below can treat it as a consonant while the
/// letters themselves survive to the end.
fn mark_non_vowels(cs: &mut [char]) {
    let n = cs.len();
    for i in 0..n {
        let c = cs[i];
        let prev = if i > 0 { Some(cs[i - 1]) } else { None };
        let next = cs.get(i + 1).copied();
        let between = prev.is_some_and(is_vowel) && next.is_some_and(is_vowel);
        match c {
            'u' | 'i' if between => cs[i] = if c == 'u' { 'U' } else { 'I' },
            'y' if prev.is_some_and(is_vowel) || next.is_some_and(is_vowel) => cs[i] = 'Y',
            'u' if prev == Some('q') => cs[i] = 'U',
            _ => {}
        }
    }
}

fn unmark(cs: &[char]) -> String {
    cs.iter()
        .map(|c| match c {
            'U' => 'u',
            'I' => 'i',
            'Y' => 'y',
            other => *other,
        })
        .collect()
}

/// `(rv, r1, r2)` as char indices.
///
/// RV is French's own: after the third letter when the word begins with
/// two vowels, otherwise after the first vowel that is not the very
/// first letter — and `par`, `col`, `tap` are the algorithm's named
/// exceptions, where RV is 3.
fn regions(cs: &[char]) -> (usize, usize, usize) {
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
    let word: String = cs.iter().collect();
    let mut rv = n;
    if n >= 3 {
        if (is_vowel(cs[0]) && is_vowel(cs[1]))
            || word.starts_with("par")
            || word.starts_with("col")
            || word.starts_with("tap")
        {
            rv = 3;
        } else {
            let mut i = 1;
            while i < n && !is_vowel(cs[i]) {
                i += 1;
            }
            rv = (i + 1).min(n);
        }
    }
    (rv.min(n), r1.min(n), r2.min(n))
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

/// Step 1 — the standard suffixes. Returns whether anything was cut,
/// because step 2 only runs when this one found nothing.
#[allow(clippy::too_many_lines)]
fn step1(cs: &mut Vec<char>, r1: usize, r2: usize, rv: usize) -> bool {
    // The plain R2 group.
    const R2_DELETE: &[&str] = &[
        "ances", "iqUes", "ismes", "ables", "istes", "ance", "iqUe", "isme", "able", "iste", "eux",
        "euse", "euses", "atrices", "atrice", "ateurs", "ations", "ateur", "ation", "logies",
        "logie", "usions", "utions", "usion", "ution", "ences", "ence", "ements", "ement", "ités",
        "ité", "ifs", "ives", "if", "ive", "aux", "eaux", "ements",
    ];
    if let Some(s) = longest_in(cs, &["ances", "iqUes", "ismes", "ables", "istes"], r2) {
        cut(cs, s);
        return true;
    }
    if let Some(s) = longest_in(
        cs,
        &["atrice", "ateur", "ation", "atrices", "ateurs", "ations"],
        r2,
    ) {
        cut(cs, s);
        if ends_with(cs, "ic") {
            if in_region(cs, 2, r2) {
                cut(cs, "ic");
            } else {
                cs.truncate(cs.len() - 2);
                cs.extend("iqU".chars());
            }
        }
        return true;
    }
    if let Some(s) = longest_in(cs, &["logie", "logies"], r2) {
        cut(cs, s);
        cs.extend("log".chars());
        return true;
    }
    if let Some(s) = longest_in(cs, &["usion", "ution", "usions", "utions"], r2) {
        cut(cs, s);
        cs.push('u');
        return true;
    }
    if let Some(s) = longest_in(cs, &["ence", "ences"], r2) {
        cut(cs, s);
        cs.extend("ent".chars());
        return true;
    }
    if let Some(s) = longest_in(cs, &["ements", "ement"], rv) {
        cut(cs, s);
        if ends_with(cs, "iv") && in_region(cs, 2, r2) {
            cut(cs, "iv");
            if ends_with(cs, "at") && in_region(cs, 2, r2) {
                cut(cs, "at");
            }
        } else if ends_with(cs, "eus") {
            if in_region(cs, 3, r2) {
                cut(cs, "eus");
            } else if in_region(cs, 3, r1) {
                cs.truncate(cs.len() - 1);
                cs.push('x');
            }
        } else if (ends_with(cs, "abl") || ends_with(cs, "iqU")) && in_region(cs, 3, r2) {
            cs.truncate(cs.len() - 3);
        } else if (ends_with(cs, "ièr") || ends_with(cs, "Ièr")) && in_region(cs, 3, rv) {
            cs.truncate(cs.len() - 3);
            cs.push('i');
        }
        return true;
    }
    if let Some(s) = longest_in(cs, &["ités", "ité"], r2) {
        cut(cs, s);
        if ends_with(cs, "abil") {
            if in_region(cs, 4, r2) {
                cut(cs, "abil");
            } else {
                cs.truncate(cs.len() - 2);
                cs.push('l');
            }
        } else if ends_with(cs, "ic") {
            if in_region(cs, 2, r2) {
                cut(cs, "ic");
            } else {
                cs.truncate(cs.len() - 2);
                cs.extend("iqU".chars());
            }
        } else if ends_with(cs, "iv") && in_region(cs, 2, r2) {
            cut(cs, "iv");
        }
        return true;
    }
    if let Some(s) = longest_in(cs, &["ifs", "ives", "if", "ive"], r2) {
        cut(cs, s);
        if ends_with(cs, "at") && in_region(cs, 2, r2) {
            cut(cs, "at");
            if ends_with(cs, "ic") {
                if in_region(cs, 2, r2) {
                    cut(cs, "ic");
                } else {
                    cs.truncate(cs.len() - 2);
                    cs.extend("iqU".chars());
                }
            }
        }
        return true;
    }
    if longest_in(cs, &["eaux"], 0).is_some() {
        cs.truncate(cs.len() - 1);
        return true;
    }
    if longest_in(cs, &["aux"], r1).is_some() {
        cs.truncate(cs.len() - 2);
        cs.push('l');
        return true;
    }
    if let Some(s) = longest_in(cs, &["euse", "euses"], r2) {
        cut(cs, s);
        return true;
    }
    if let Some(s) = longest_in(cs, &["euse", "euses"], r1) {
        cut(cs, s);
        cs.push('x');
        return true;
    }
    if let Some(s) = longest_in(cs, R2_DELETE, r2) {
        cut(cs, s);
        return true;
    }
    // `amment` / `emment` become `ant` / `ent` in RV.
    if ends_with(cs, "amment") && in_region(cs, 6, rv) {
        cut(cs, "amment");
        cs.extend("ant".chars());
        return true;
    }
    if ends_with(cs, "emment") && in_region(cs, 6, rv) {
        cut(cs, "emment");
        cs.extend("ent".chars());
        return true;
    }
    // `ment` / `ments` preceded by a vowel, in RV. The vowel has to be
    // in RV, not the suffix — and it goes too, which is the half I left
    // out: `bâtiment` stems to `bât` on PG 18.4, not `bâti`.
    for s in ["ments", "ment"] {
        let l = s.chars().count();
        if ends_with(cs, s) && cs.len() > l && is_vowel(cs[cs.len() - l - 1]) && cs.len() - l > rv {
            cut(cs, s);
            // ...and then the `i`-verb rule gets its turn, which is the
            // half a reading of the algorithm alone would miss. Asked
            // for twenty-five `-ment` words, PG 18.4 answers `bâtiment`
            // → `bât`, `sentiment` → `sent` and `régiment` → `reg`,
            // but `document` → `docu` and `vraiment` → `vrai`. What
            // separates them is not the region: it is that the first
            // three end in an `i` preceded by a consonant once `ment`
            // is gone, which is exactly what `step2a` removes. `docu`
            // ends in `u`, and `vrai`'s `i` sits behind a vowel.
            if cs.last() == Some(&'i') && cs.len() >= 2 && !is_vowel(cs[cs.len() - 2]) {
                cs.pop();
            }
            return true;
        }
    }
    false
}

const STEP2A: &[&str] = &[
    "issaIent", "issantes", "iraIent", "issance", "issions", "issants", "issions", "issait",
    "issant", "issent", "issiez", "issons", "irions", "issais", "iraient", "irions", "issante",
    "irais", "irait", "irent", "iriez", "irons", "iront", "isses", "issez", "îmes", "îtes", "irai",
    "iras", "irez", "isse", "ies", "ira", "ît", "ie", "ir", "is", "it", "i",
];

/// Step 2a — the `i`-verb suffixes, each preceded by a non-vowel.
fn step2a(cs: &mut Vec<char>, rv: usize) -> bool {
    let Some(s) = longest_in(cs, STEP2A, rv) else {
        return false;
    };
    let l = s.chars().count();
    if cs.len() > l && !is_vowel(cs[cs.len() - l - 1]) {
        cut(cs, s);
        return true;
    }
    false
}

/// Group (b): the `er`/`é` family, deleted in RV.
const STEP2B_B: &[&str] = &[
    // The future and conditional carry the whole `er` stem with them:
    // `chanterai` is `chant` on PG 18.4, not `chanter`. Leaving these
    // out left every future tense one suffix short — fifteen words on
    // one differential run, all of them the same shape.
    "eraIent", "eraient", "erions", "erIons", "erait", "erais", "eriez", "erons", "eront", "èrent",
    "erent", "erai", "eras", "erez", "era", "ées", "iez", "ée", "és", "er", "ez", "é",
];

/// Group (c): the `a`/`ant`/`asse` family. After one of these comes
/// off, a preceding `e` in RV goes too — which is why `mangeait`
/// reaches `mang` and not `mange`.
const STEP2B_C: &[&str] = &[
    "assions", "assent", "assiez", "asses", "aIent", "antes", "âmes", "âtes", "ante", "asse",
    "ants", "ais", "ait", "ant", "ât", "ai", "as", "a",
];

/// Step 2b — Snowball's three groups, in its order.
///
/// `ions` is first and is guarded by R2 SEPARATELY from the match. When
/// the match succeeds at RV and the R2 guard fails, the whole step
/// fails rather than falling back to a shorter suffix — which is how
/// `chantions` keeps its `ion` and loses only the `s` to the residual
/// step. Treating it like the region-limited groups took it to `chant`
/// against PG 18.4's `chantion`.
fn step2b(cs: &mut Vec<char>, rv: usize, r2: usize) -> bool {
    // ONE among over all three groups, longest wins. `ions` and
    // `erions` are alternatives of the same match, so `chanterions`
    // takes the six-letter one and reaches `chant`; letting `ions` be
    // tested first left it at `chanter`.
    let ions = ends_with(cs, "ions") && in_region(cs, 4, rv);
    let b = longest_in(cs, STEP2B_B, rv);
    let c = longest_in(cs, STEP2B_C, rv);
    let best_len = [
        if ions { 4 } else { 0 },
        b.map_or(0, |s| s.chars().count()),
        c.map_or(0, |s| s.chars().count()),
    ]
    .into_iter()
    .max()
    .unwrap_or(0);
    if best_len == 0 {
        return false;
    }
    if let Some(s) = b.filter(|s| s.chars().count() == best_len) {
        cut(cs, s);
        return true;
    }
    if let Some(s) = c.filter(|s| s.chars().count() == best_len) {
        cut(cs, s);
        if ends_with(cs, "e") && in_region(cs, 1, rv) {
            cs.pop();
        }
        return true;
    }
    // `ions` won, and its delete is guarded by R2 separately. When that
    // guard fails the whole step fails rather than falling back to a
    // shorter suffix, which is how `chantions` keeps its `ion` and
    // loses only the `s` to the residual step.
    if in_region(cs, 4, r2) {
        cut(cs, "ions");
        return true;
    }
    false
}

/// The residual suffix, then unelision and de-doubling.
fn step_residual(cs: &mut Vec<char>, rv: usize, r2: usize) {
    if ends_with(cs, "s") {
        let n = cs.len();
        if n >= 2 && !matches!(cs[n - 2], 'a' | 'i' | 'o' | 'u' | 'è' | 's') {
            cs.pop();
        }
    }
    for s in ["ion"] {
        if ends_with(cs, s)
            && in_region(cs, 3, r2)
            && cs.len() >= 4
            && matches!(cs[cs.len() - 4], 's' | 't')
        {
            cut(cs, s);
            return;
        }
    }
    for s in ["ier", "ière", "Ier", "Ière"] {
        if ends_with(cs, s) && in_region(cs, s.chars().count(), rv) {
            cut(cs, s);
            cs.push('i');
            return;
        }
    }
    if ends_with(cs, "e") && in_region(cs, 1, rv) {
        cs.pop();
        return;
    }
    if ends_with(cs, "ë") && cs.len() >= 3 && cs[cs.len() - 3..cs.len() - 1] == ['g', 'u'] {
        cs.pop();
    }
}

/// Snowball's French stem of `word`, which must already be lowercase.
pub(crate) fn stem_fr(word: &str) -> String {
    let mut cs: Vec<char> = word.chars().collect();
    if cs.len() <= 2 {
        return finish(&cs);
    }
    mark_non_vowels(&mut cs);
    let (rv, r1, r2) = regions(&cs);
    let before: Vec<char> = cs.clone();
    let cut1 = step1(&mut cs, r1, r2, rv);
    if !cut1 {
        if !step2a(&mut cs, rv) {
            step2b(&mut cs, rv, r2);
        }
    }
    if cs == before {
        // Nothing came off: the residual step gets its turn.
        step_residual(&mut cs, rv, r2);
    } else {
        // `Y`/`ç` normalisation, then the doubled consonant.
        if ends_with(&cs, "Y") {
            cs.pop();
            cs.push('i');
        }
        if ends_with(&cs, "ç") {
            cs.pop();
            cs.push('c');
        }
    }
    undouble(&mut cs);
    unaccent_before_consonant(&mut cs);
    finish(&cs)
}

/// The last step, and it is a SHORT list: a word ending `enn`, `onn`,
/// `ett`, `ell` or `eill` loses its last letter.
///
/// "Any doubled consonant loses one" is what I wrote first, and it took
/// `ville` to `vil` and `chatte` to `chat` where PG 18.4 says `vill`
/// and `chatt`. The rule is about those five endings, not about
/// doubling.
fn undouble(cs: &mut Vec<char>) {
    for suf in ["eill", "enn", "onn", "ett", "ell"] {
        if ends_with(cs, suf) {
            cs.pop();
            return;
        }
    }
}

/// v7.38.18 — an `é`/`è` FOLLOWED BY CONSONANTS to the end becomes `e`.
///
/// Measured on PG 18.4, and the pair that fixes the rule in place:
/// `considérer` stems to `consider` while `créer` stems to `cré`. In
/// the first the accented letter is followed by an `r`; in the second
/// it ends the word. `vérité` stems to `vérit`, so this is not "strip
/// accents" — that gives `verit` — and it is not "the final letter"
/// either, which was my second guess and took `allé` to `alle`.
fn unaccent_before_consonant(cs: &mut [char]) {
    let Some(pos) = cs.iter().rposition(|c| matches!(c, 'é' | 'è')) else {
        return;
    };
    if pos + 1 < cs.len() && cs[pos + 1..].iter().all(|c| !is_vowel(*c)) {
        cs[pos] = 'e';
    }
}

/// v7.38.18 — French keeps its accents, and that is not an oversight.
/// PG 18.4 stems `vérité` to `vérit` and `général` to `général`, where
/// Spanish stems `corazón` to `corazon`. Deaccenting here — copied from
/// the Spanish stemmer, where it belongs — cost four words on the first
/// differential run.
fn finish(cs: &[char]) -> String {
    unmark(cs)
}
