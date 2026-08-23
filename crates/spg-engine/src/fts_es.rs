//! v7.38.18 — the Spanish stemmer, from Snowball's published algorithm.
//!
//! Implemented from the algorithm's own description and verified word
//! for word against PostgreSQL 18.4 over a 914-word vocabulary built to
//! reach every suffix class it names. What the oracle decides is the
//! spec; nothing here is transcribed from anyone's source.
//!
//! The shape is Snowball's: three regions (`RV`, `R1`, `R2`) computed
//! once, then attached-pronoun removal, then the standard/y/verb suffix
//! steps in the order the algorithm fixes, then residual, then the
//! accents come off. The last step is why `árboles` stems to `arbol`
//! and `corazón` to `corazon`, while `niños` keeps its `ñ` — `ñ` is a
//! letter in Spanish, not an accented `n`.

use alloc::string::String;
use alloc::vec::Vec;

fn is_vowel(c: char) -> bool {
    matches!(
        c,
        'a' | 'e' | 'i' | 'o' | 'u' | 'á' | 'é' | 'í' | 'ó' | 'ú' | 'ü'
    )
}

/// `(rv, r1, r2)` as char indices into `cs`.
///
/// R1 is after the first vowel-then-consonant; R2 the same inside R1.
/// RV is Spanish's own: if the second letter is a consonant, after the
/// next vowel; if the first two are both vowels, after the next
/// consonant; otherwise after the third letter.
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
    let mut rv = n;
    if n > 3 {
        if !is_vowel(cs[1]) {
            let mut i = 2;
            while i < n && !is_vowel(cs[i]) {
                i += 1;
            }
            rv = (i + 1).min(n);
        } else if is_vowel(cs[0]) && is_vowel(cs[1]) {
            let mut i = 2;
            while i < n && is_vowel(cs[i]) {
                i += 1;
            }
            rv = (i + 1).min(n);
        } else {
            rv = 3;
        }
    }
    (rv.min(n), r1.min(n), r2.min(n))
}

fn ends_with(cs: &[char], suf: &str) -> bool {
    let s: Vec<char> = suf.chars().collect();
    cs.len() >= s.len() && cs[cs.len() - s.len()..] == s[..]
}

/// Does a suffix of this length start at or after `region`?
fn in_region(cs: &[char], suf_len: usize, region: usize) -> bool {
    cs.len() >= suf_len && cs.len() - suf_len >= region
}

/// The longest suffix in `sufs` that `cs` ends with AND that lies
/// inside `region`.
///
/// The region is part of the search, not a test applied afterwards.
/// Snowball expresses these steps as an `among` inside a `setlimit`,
/// so a longer suffix that falls outside the region does not shadow a
/// shorter one that fits. Taking the longest first and then checking
/// cost four words out of 898 against PG 18.4 — `queremos` stopped at
/// `querem` because `eremos` reached past RV and hid `emos`, and
/// `acabas` was left whole because `abas` hid `as`.
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
    let n = suf.chars().count();
    cs.truncate(cs.len() - n);
}

const PRONOUNS: &[&str] = &[
    "selas", "selos", "sela", "selo", "las", "les", "los", "nos", "me", "se", "la", "le", "lo",
];

/// Step 0 — an attached pronoun, but only after one of the verb forms
/// that can carry one.
fn step0(cs: &mut Vec<char>, rv: usize) {
    let Some(p) = longest_in(cs, PRONOUNS, rv) else {
        return;
    };
    let plen = p.chars().count();
    let before = &cs[..cs.len() - plen];
    // `iéndo`, `ándo`, `ár`, `ér`, `ír` keep their accent when the
    // pronoun comes off; `ando`, `iendo`, `ar`, `er`, `ir` and `yendo`
    // (after a `u`) do not.
    let accented: &[&str] = &["iéndo", "ándo", "ár", "ér", "ír"];
    let plain: &[&str] = &["ando", "iendo", "ar", "er", "ir"];
    let bs: Vec<char> = before.to_vec();
    if let Some(a) = longest_in(&bs, accented, rv) {
        {
            cut(cs, p);
            let keep = cs.len() - a.chars().count();
            let deaccent: String = cs[keep..]
                .iter()
                .map(|c| match c {
                    'á' => 'a',
                    'é' => 'e',
                    'í' => 'i',
                    'ó' => 'o',
                    'ú' => 'u',
                    other => *other,
                })
                .collect();
            cs.truncate(keep);
            cs.extend(deaccent.chars());
        }
        return;
    }
    if longest_in(&bs, plain, rv).is_some() {
        cut(cs, p);
        return;
    }
    // `uyendo` — the pronoun comes off after `yendo` preceded by `u`.
    if ends_with(&bs, "yendo") && bs.len() >= 6 && bs[bs.len() - 6] == 'u' {
        cut(cs, p);
    }
}

const STEP1_R2: &[&str] = &[
    "amientos", "imientos", "amiento", "imiento", "aciones", "uciones", "ativas", "ativos",
    "adoras", "adores", "ancias", "encias", "idades", "amente", "antes", "ación", "ución", "ativa",
    "ativo", "adora", "ador", "ancia", "encia", "ibles", "ismos", "istas", "ivas", "ivos", "anza",
    "anzas", "ante", "icos", "icas", "ismo", "able", "ables", "ible", "ista", "osos", "osas",
    "oso", "osa", "iva", "ivo", "ico", "ica", "idad",
];

/// Step 1 — the standard suffixes. Each group has its own region and a
/// couple have a follow-up cut, which is where `logías` → `log` and
/// `amente` → its own two-stage removal come from.
fn step1(cs: &mut Vec<char>, r1: usize, r2: usize) -> bool {
    // v7.38.18 — the REPLACEMENT groups first. They are not "delete a
    // suffix", they are "delete and put something back", and the plain
    // group below contains `encias` / `ancia`, which would swallow them:
    // `competencia` came out `compet` where PG 18.4 says `competent`.
    for (suf, follow) in [
        ("logías", "log"),
        ("logía", "log"),
        ("uciones", "u"),
        ("ución", "u"),
        ("encias", "ente"),
        ("encia", "ente"),
    ] {
        if ends_with(cs, suf) && in_region(cs, suf.chars().count(), r2) {
            cut(cs, suf);
            cs.extend(follow.chars());
            return true;
        }
    }
    // `idad` / `idades` take a second cut of `abil` / `ic` / `iv`,
    // which the plain group cannot express either:
    // `responsabilidad` came out `responsabil` against PG's `respons`.
    for suf in ["idades", "idad"] {
        if ends_with(cs, suf) && in_region(cs, suf.chars().count(), r2) {
            cut(cs, suf);
            for tail in ["abil", "ic", "iv"] {
                if ends_with(cs, tail) && in_region(cs, tail.chars().count(), r2) {
                    cut(cs, tail);
                    break;
                }
            }
            return true;
        }
    }
    // The groups that simply come off in R2.
    if let Some(s) = longest_in(cs, STEP1_R2, r2) {
        {
            cut(cs, s);
            // `adora`/`ador`/`ación` leave an `ic` that also goes in R2.
            if matches!(
                s,
                "adora" | "ador" | "ación" | "aciones" | "adores" | "adoras"
            ) && ends_with(cs, "ic")
                && in_region(cs, 2, r2)
            {
                cut(cs, "ic");
            }
            // `amente` in R1 leaves `iv`/`os`/`ic`/`ad` behind.
            return true;
        }
    }
    if let Some(s) = longest_in(cs, &["amente"], r1) {
        {
            cut(cs, s);
            for tail in ["ativ", "iv", "os", "ic", "ad"] {
                if ends_with(cs, tail) && in_region(cs, tail.chars().count(), r2) {
                    cut(cs, tail);
                    break;
                }
            }
            return true;
        }
    }
    for (suf, region, follow) in [
        ("logías", r2, Some("log")),
        ("logía", r2, Some("log")),
        ("ución", r2, Some("u")),
        ("uciones", r2, Some("u")),
        ("encias", r2, Some("ente")),
        ("encia", r2, Some("ente")),
    ] {
        if ends_with(cs, suf) && in_region(cs, suf.chars().count(), region) {
            cut(cs, suf);
            if let Some(f) = follow {
                cs.extend(f.chars());
            }
            return true;
        }
    }
    for suf in ["mente"] {
        if ends_with(cs, suf) && in_region(cs, suf.chars().count(), r1) {
            cut(cs, suf);
            for tail in ["ante", "able", "ible"] {
                if ends_with(cs, tail) && in_region(cs, tail.chars().count(), r2) {
                    cut(cs, tail);
                    break;
                }
            }
            return true;
        }
    }
    for suf in ["idades", "idad"] {
        if ends_with(cs, suf) && in_region(cs, suf.chars().count(), r2) {
            cut(cs, suf);
            for tail in ["abil", "ic", "iv"] {
                if ends_with(cs, tail) && in_region(cs, tail.chars().count(), r2) {
                    cut(cs, tail);
                    break;
                }
            }
            return true;
        }
    }
    for suf in ["ivas", "ivos", "iva", "ivo"] {
        if ends_with(cs, suf) && in_region(cs, suf.chars().count(), r2) {
            cut(cs, suf);
            if ends_with(cs, "at") && in_region(cs, 2, r2) {
                cut(cs, "at");
            }
            return true;
        }
    }
    false
}

const STEP2A: &[&str] = &[
    "yeron", "yendo", "yamos", "yais", "yan", "yen", "yas", "yes", "ya", "ye", "yo", "yó",
];

/// Step 2a — the `y` verb suffixes, removed only after a `u`.
fn step2a(cs: &mut Vec<char>, rv: usize) -> bool {
    let Some(s) = longest_in(cs, STEP2A, rv) else {
        return false;
    };
    let l = s.chars().count();
    if cs.len() > l && cs[cs.len() - l - 1] == 'u' {
        cut(cs, s);
        return true;
    }
    false
}

const STEP2B_A: &[&str] = &[
    "aríamos", "eríamos", "iríamos", "iéramos", "iésemos", "aríais", "aremos", "eríais", "eremos",
    "iríais", "iremos", "ierais", "ieseis", "asteis", "isteis", "ábamos", "áramos", "ásemos",
    "arían", "arías", "aréis", "erían", "erías", "eréis", "irían", "irías", "iréis", "ieran",
    "iesen", "ieron", "iendo", "ieras", "ieses", "abais", "arais", "aseis", "éamos", "arán",
    "arás", "aría", "erán", "erás", "ería", "irán", "irás", "iría", "iera", "iese", "aste", "iste",
    "aban", "aran", "asen", "aron", "ando", "abas", "adas", "idas", "aras", "ases", "íais", "ados",
    "idos", "amos", "imos", "emos", "ará", "aré", "erá", "eré", "irá", "iré", "aba", "ada", "ida",
    "ara", "ase", "ían", "ado", "ido", "ías", "áis", "éis", "ía", "ad", "ed", "id", "an", "ió",
    "ar", "er", "ir", "as", "ís", "en", "es",
];

/// Step 2b — the rest of the verb suffixes. `en`/`es`/`éis`/`emos`
/// leave a `gu` that loses its `u`, which is why `siguen` → `sigu` →
/// `sig`.
fn step2b(cs: &mut Vec<char>, rv: usize) -> bool {
    let Some(s) = longest_in(cs, STEP2B_A, rv) else {
        return false;
    };
    cut(cs, s);
    if matches!(s, "en" | "es" | "éis" | "emos") && ends_with(cs, "gu") && in_region(cs, 1, rv) {
        cs.pop();
    }
    true
}

/// Step 3 — the residual suffix, then the accents.
fn step3(cs: &mut Vec<char>, rv: usize) {
    for s in ["os", "a", "o", "á", "í", "ó"] {
        if ends_with(cs, s) && in_region(cs, s.chars().count(), rv) {
            cut(cs, s);
            return;
        }
    }
    for s in ["e", "é"] {
        if ends_with(cs, s) && in_region(cs, 1, rv) {
            cut(cs, s);
            // A `gu` left behind loses its `u` when the `u` is in RV.
            if ends_with(cs, "gu") && in_region(cs, 1, rv) {
                cs.pop();
            }
            return;
        }
    }
}

/// Snowball's Spanish stem of `word`, which must already be lowercase.
pub(crate) fn stem_es(word: &str) -> String {
    let mut cs: Vec<char> = word.chars().collect();
    if cs.len() <= 2 {
        return deaccent(&cs);
    }
    let (rv, r1, r2) = regions(&cs);
    step0(&mut cs, rv);
    let before: Vec<char> = cs.clone();
    if !step1(&mut cs, r1, r2) {
        // Step 2 only runs when step 1 removed nothing.
        if !step2a(&mut cs, rv) {
            step2b(&mut cs, rv);
        }
    }
    let _ = before;
    step3(&mut cs, rv);
    deaccent(&cs)
}

/// The last step: acute accents come off, `ñ` and `ü` stay. `ü` is a
/// diaeresis rather than an accent and `ñ` is its own letter.
fn deaccent(cs: &[char]) -> String {
    cs.iter()
        .map(|c| match c {
            'á' => 'a',
            'é' => 'e',
            'í' => 'i',
            'ó' => 'o',
            'ú' => 'u',
            other => *other,
        })
        .collect()
}
