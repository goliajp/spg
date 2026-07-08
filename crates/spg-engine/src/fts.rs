//! v7.12.1 — full-text search lexer / stemmer.
//!
//! Powers `to_tsvector`, `plainto_tsquery`, `to_tsquery`, and
//! friends. Two configs are supported in v7.12:
//!   - `simple` — lowercase + tokenise; no stopwords, no stemming.
//!   - `english` — lowercase + tokenise + drop PG-standard
//!     english stopwords + Porter v1 stem.
//!
//! Other configs (`spanish`, `german`, `russian`, …) error with
//! `EvalError::TypeMismatch` carrying the unsupported-config name
//! so callers see the same shape as `::regtype` rejection.
//!
//! Porter stemmer implementation follows the original 1980
//! Algorithm; corner-case behaviour matches Snowball english v1
//! (the variant PG also uses).

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_storage::{TsLexeme, TsQueryAst};

use crate::eval::EvalError;

/// v7.12.1 — supported tokeniser configs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsConfig {
    /// `simple` / `pg_catalog.simple` — lowercase + split, no
    /// stopword drop, no stem.
    Simple,
    /// `english` / `pg_catalog.english` — lowercase + split +
    /// stopword drop + Porter v1 stem.
    English,
}

impl TsConfig {
    /// Resolve a PG text-search config name. The PG-qualified
    /// form `pg_catalog.<name>` is accepted too. Returns `None`
    /// for any other name so the caller can produce a clear
    /// "config not implemented" error listing what is supported.
    pub fn from_name(name: &str) -> Option<Self> {
        let bare = name.strip_prefix("pg_catalog.").unwrap_or(name);
        match bare.to_ascii_lowercase().as_str() {
            "simple" => Some(Self::Simple),
            "english" => Some(Self::English),
            _ => None,
        }
    }
}

/// v7.12.1 — tokenise + (optionally) stem `text` into a sorted +
/// deduped lexeme set with merged positions. Each token's
/// position is 1-based and capped at u16::MAX (matching PG's
/// `MaxTSPosition`).
pub fn to_tsvector(config: TsConfig, text: &str) -> Vec<TsLexeme> {
    let mut out: Vec<TsLexeme> = Vec::new();
    let mut position: u16 = 0;
    for token in tokenize(text) {
        let lex = match config {
            TsConfig::Simple => token,
            TsConfig::English => {
                if is_english_stopword(&token) {
                    // PG drops stopwords from the vector but
                    // still increments position so phrase
                    // distances stay meaningful.
                    position = position.saturating_add(1);
                    continue;
                }
                porter_stem(&token)
            }
        };
        if lex.is_empty() {
            continue;
        }
        position = position.saturating_add(1);
        match out.binary_search_by(|l| l.word.as_str().cmp(lex.as_str())) {
            Ok(idx) => {
                if !out[idx].positions.contains(&position) {
                    out[idx].positions.push(position);
                }
            }
            Err(idx) => {
                out.insert(
                    idx,
                    TsLexeme {
                        word: lex,
                        positions: alloc::vec![position],
                        weight: 0,
                    },
                );
            }
        }
    }
    out
}

/// v7.12.1 — `plainto_tsquery(config, text)`: tokenise + stem,
/// fold the surviving lexemes into an AND tree. Returns
/// `EvalError::TypeMismatch` only for an unsupported config — an
/// all-stopwords input becomes an empty `Term("")` so the caller
/// can detect it.
pub fn plainto_tsquery(config: TsConfig, text: &str) -> TsQueryAst {
    let lexs = collect_lexemes(config, text);
    fold_and(&lexs)
}

/// v7.12.1 — `phraseto_tsquery(config, text)`: same tokenise + stem,
/// but preserve order — fold into nested `Phrase(_, _, 1)` nodes.
pub fn phraseto_tsquery(config: TsConfig, text: &str) -> TsQueryAst {
    let lexs = collect_lexemes(config, text);
    fold_phrase(&lexs)
}

/// v7.12.1 — `websearch_to_tsquery(config, text)`: Google-style
/// syntax. Quoted phrases → phrase node; `OR` (case-insensitive)
/// → OR; leading `-` → NOT; otherwise AND.
///
/// The grammar is liberal — malformed input degrades to a plain
/// AND of the bare tokens, matching PG semantics.
pub fn websearch_to_tsquery(config: TsConfig, text: &str) -> TsQueryAst {
    let mut tokens = web_tokens(text);
    // Apply config to each plain term + each phrase part.
    for t in &mut tokens {
        match t {
            WebToken::Term(s) | WebToken::NotTerm(s) => {
                let lexs = collect_lexemes(config, s);
                *s = lexs.join(" ");
            }
            WebToken::Phrase(words) => {
                let mut combined = String::new();
                for w in words.iter() {
                    if !combined.is_empty() {
                        combined.push(' ');
                    }
                    combined.push_str(w);
                }
                let lexs = collect_lexemes(config, &combined);
                *words = lexs;
            }
            WebToken::Or => {}
        }
    }
    // Group by OR boundaries; within each group AND together.
    let mut or_groups: Vec<Vec<TsQueryAst>> = alloc::vec![Vec::new()];
    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            WebToken::Or => {
                or_groups.push(Vec::new());
            }
            WebToken::Term(s) => {
                if !s.is_empty() {
                    let node = fold_and(&split_words(s));
                    or_groups.last_mut().unwrap().push(node);
                }
            }
            WebToken::NotTerm(s) => {
                if !s.is_empty() {
                    let node = TsQueryAst::Not(alloc::boxed::Box::new(fold_and(&split_words(s))));
                    or_groups.last_mut().unwrap().push(node);
                }
            }
            WebToken::Phrase(words) => {
                if !words.is_empty() {
                    or_groups.last_mut().unwrap().push(fold_phrase(words));
                }
            }
        }
        i += 1;
    }
    let group_nodes: Vec<TsQueryAst> = or_groups
        .into_iter()
        .filter_map(|g| {
            if g.is_empty() {
                None
            } else {
                let mut it = g.into_iter();
                let first = it.next().unwrap();
                Some(it.fold(first, |acc, n| {
                    TsQueryAst::And(alloc::boxed::Box::new(acc), alloc::boxed::Box::new(n))
                }))
            }
        })
        .collect();
    if group_nodes.is_empty() {
        return TsQueryAst::Term {
            word: String::new(),
            weight_mask: 0,
        };
    }
    let mut it = group_nodes.into_iter();
    let first = it.next().unwrap();
    it.fold(first, |acc, n| {
        TsQueryAst::Or(alloc::boxed::Box::new(acc), alloc::boxed::Box::new(n))
    })
}

/// v7.12.1 — `to_tsquery(config, text)`: explicit operator syntax
/// over already-stemmed terms. Reuses the v7.12.0 external-form
/// parser, then walks each leaf through `porter_stem` (when the
/// config is `english`). Returns `TypeMismatch` on malformed input.
pub fn to_tsquery(config: TsConfig, text: &str) -> Result<TsQueryAst, EvalError> {
    let mut ast = crate::eval::decode_tsquery_external(text)?;
    stem_tsquery_in_place(&mut ast, config);
    Ok(ast)
}

fn stem_tsquery_in_place(ast: &mut TsQueryAst, config: TsConfig) {
    match ast {
        TsQueryAst::Term { word, .. } => {
            let lower = word.to_lowercase();
            *word = match config {
                TsConfig::Simple => lower,
                TsConfig::English => porter_stem(&lower),
            };
        }
        TsQueryAst::And(a, b) | TsQueryAst::Or(a, b) => {
            stem_tsquery_in_place(a, config);
            stem_tsquery_in_place(b, config);
        }
        TsQueryAst::Not(x) => stem_tsquery_in_place(x, config),
        TsQueryAst::Phrase { left, right, .. } => {
            stem_tsquery_in_place(left, config);
            stem_tsquery_in_place(right, config);
        }
    }
}

fn collect_lexemes(config: TsConfig, text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for token in tokenize(text) {
        match config {
            TsConfig::Simple => out.push(token),
            TsConfig::English => {
                if is_english_stopword(&token) {
                    continue;
                }
                let stemmed = porter_stem(&token);
                if !stemmed.is_empty() {
                    out.push(stemmed);
                }
            }
        }
    }
    out
}

fn split_words(s: &str) -> Vec<String> {
    s.split_whitespace().map(|w| w.to_string()).collect()
}

fn fold_and(lexs: &[String]) -> TsQueryAst {
    if lexs.is_empty() {
        return TsQueryAst::Term {
            word: String::new(),
            weight_mask: 0,
        };
    }
    let mut it = lexs.iter();
    let first = TsQueryAst::Term {
        word: it.next().unwrap().clone(),
        weight_mask: 0,
    };
    it.fold(first, |acc, w| {
        TsQueryAst::And(
            alloc::boxed::Box::new(acc),
            alloc::boxed::Box::new(TsQueryAst::Term {
                word: w.clone(),
                weight_mask: 0,
            }),
        )
    })
}

fn fold_phrase(lexs: &[String]) -> TsQueryAst {
    if lexs.is_empty() {
        return TsQueryAst::Term {
            word: String::new(),
            weight_mask: 0,
        };
    }
    let mut it = lexs.iter();
    let first = TsQueryAst::Term {
        word: it.next().unwrap().clone(),
        weight_mask: 0,
    };
    it.fold(first, |acc, w| TsQueryAst::Phrase {
        left: alloc::boxed::Box::new(acc),
        right: alloc::boxed::Box::new(TsQueryAst::Term {
            word: w.clone(),
            weight_mask: 0,
        }),
        distance: 1,
    })
}

/// v7.12.2 — evaluate `tsvector @@ tsquery`. Walks the query AST
/// treating each leaf as "does the vector contain this lexeme".
/// Phrase semantics: the v7.12.2 implementation honours the
/// `<N>` distance — both operand terms must appear with their
/// positions exactly `N` apart in the vector. Higher-arity
/// phrase chains nest as `Phrase(Phrase(a,b,1), c, 1)`, so the
/// match recursion folds position sets across the AND of the
/// chain (a fully general n-gram match in a single pass).
#[must_use]
pub fn ts_query_matches(vec: &[TsLexeme], query: &TsQueryAst) -> bool {
    match query {
        TsQueryAst::Term { word, .. } => contains_lexeme(vec, word),
        TsQueryAst::And(a, b) => ts_query_matches(vec, a) && ts_query_matches(vec, b),
        TsQueryAst::Or(a, b) => ts_query_matches(vec, a) || ts_query_matches(vec, b),
        TsQueryAst::Not(x) => !ts_query_matches(vec, x),
        TsQueryAst::Phrase {
            left,
            right,
            distance,
        } => phrase_match(vec, left, right, *distance),
    }
}

fn contains_lexeme(vec: &[TsLexeme], word: &str) -> bool {
    vec.binary_search_by(|l| l.word.as_str().cmp(word)).is_ok()
}

/// Phrase positions of a sub-AST. For atomic terms returns the
/// vector's recorded positions; for nested phrases returns the
/// rightmost position of each surviving match. Empty positions
/// mean "no match anywhere".
fn phrase_positions(vec: &[TsLexeme], q: &TsQueryAst) -> Vec<u16> {
    match q {
        TsQueryAst::Term { word, .. } => {
            match vec.binary_search_by(|l| l.word.as_str().cmp(word)) {
                Ok(idx) => vec[idx].positions.clone(),
                Err(_) => Vec::new(),
            }
        }
        TsQueryAst::Phrase {
            left,
            right,
            distance,
        } => {
            let lp = phrase_positions(vec, left);
            let rp = phrase_positions(vec, right);
            let mut out = Vec::new();
            for l in &lp {
                let target = l.saturating_add(*distance);
                if rp.binary_search(&target).is_ok() {
                    out.push(target);
                }
            }
            out.sort_unstable();
            out.dedup();
            out
        }
        // For mixed-shape phrases (Phrase contains an AND/OR/NOT),
        // fall back to the boolean match (no position tracking).
        _ => {
            if ts_query_matches(vec, q) {
                alloc::vec![u16::MAX]
            } else {
                Vec::new()
            }
        }
    }
}

fn phrase_match(vec: &[TsLexeme], left: &TsQueryAst, right: &TsQueryAst, distance: u16) -> bool {
    let lp = phrase_positions(vec, left);
    let rp = phrase_positions(vec, right);
    lp.iter().any(|l| {
        let target = l.saturating_add(distance);
        rp.binary_search(&target).is_ok()
    })
}

/// v7.12.2 — `ts_rank(vec, q)` basic form. Score is the sum of
/// per-matched-lexeme weight factors divided by `1 + log(unique
/// terms in query)`. Matches PG's `ts_rank` with default
/// normalisation flag 0.
#[must_use]
pub fn ts_rank(vec: &[TsLexeme], query: &TsQueryAst) -> f32 {
    // v7.38 (read01, T12.1) — PG's calc_rank: an AND/PHRASE-rooted query uses
    // the cover (distance-weighted) branch, everything else the OR branch.
    // Normalization flag defaults to 0 (no length/uniqueness division).
    let mut terms: Vec<&str> = Vec::new();
    collect_query_terms(query, &mut terms);
    if terms.is_empty() {
        return 0.0;
    }
    let and_rooted = matches!(query, TsQueryAst::And(..) | TsQueryAst::Phrase { .. });
    // calc_rank_and delegates to calc_rank_or for a single distinct term.
    if and_rooted && terms.len() >= 2 {
        calc_rank_and(vec, &terms)
    } else {
        calc_rank_or(vec, &terms)
    }
}

/// v7.12.2 — `ts_rank_cd(vec, q)` cover-density variant. Higher
/// score when matched lexemes cluster closer together; defaults
/// to a per-lexeme contribution divided by the average gap
/// between matched positions. Returns 0 when no terms match.
#[must_use]
pub fn ts_rank_cd(vec: &[TsLexeme], query: &TsQueryAst) -> f32 {
    let mut matched_positions: Vec<u16> = Vec::new();
    let mut score = 0.0f32;
    let mut unique_terms = 0usize;
    collect_cd_positions(
        query,
        vec,
        &mut matched_positions,
        &mut score,
        &mut unique_terms,
    );
    if matched_positions.is_empty() || unique_terms == 0 {
        return 0.0;
    }
    matched_positions.sort_unstable();
    matched_positions.dedup();
    // Cover density: invert the average distance between
    // consecutive matched positions.
    if matched_positions.len() == 1 {
        return score / (1.0 + ln_approx(unique_terms as f32));
    }
    let gaps: u32 = matched_positions
        .windows(2)
        .map(|w| u32::from(w[1] - w[0]))
        .sum();
    let avg_gap = (gaps as f32) / ((matched_positions.len() - 1) as f32);
    let density = 1.0 / avg_gap.max(1.0);
    score * density / (1.0 + ln_approx(unique_terms as f32))
}

/// `f32::ln` is std-only; spg-engine is no_std. Reuse the bit-
/// trick decomposition the spg-storage bloom filter uses
/// (precision ≈ 1e-7, ample for ranking).
fn ln_approx(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    let xd = f64::from(x);
    let bits = xd.to_bits();
    let exponent_raw = ((bits >> 52) & 0x7ff) as i64;
    let exponent = exponent_raw - 1023;
    let mantissa_bits = (bits & 0x000f_ffff_ffff_ffff) | 0x3ff0_0000_0000_0000;
    let mantissa = f64::from_bits(mantissa_bits);
    let t = (mantissa - 1.0) / (mantissa + 1.0);
    let t2 = t * t;
    let ln_mantissa = 2.0 * (t + t2 * t / 3.0 + t2 * t2 * t / 5.0 + t2 * t2 * t2 * t / 7.0);
    let ln = (exponent as f64) * core::f64::consts::LN_2 + ln_mantissa;
    ln as f32
}

fn weight_factor(w: u8) -> f32 {
    // PG default: A=1.0, B=0.4, C=0.2, D=0.1.
    match w {
        3 => 1.0,
        2 => 0.4,
        1 => 0.2,
        _ => 0.1,
    }
}

/// v7.38 (read01, T12.1) — no_std `exp`, sibling of `ln_approx`. Range-reduce
/// `x = k·ln2 + r` and evaluate `2^k · e^r` with a Taylor series on the small
/// remainder; saturate the far tails.
fn exp_approx(x: f32) -> f32 {
    if x > 88.0 {
        return f32::INFINITY;
    }
    if x < -88.0 {
        return 0.0;
    }
    let xd = f64::from(x);
    let k = (xd / core::f64::consts::LN_2).round();
    let r = xd - k * core::f64::consts::LN_2;
    // e^r, r in [-ln2/2, ln2/2]; 7 Taylor terms.
    let mut term = 1.0f64;
    let mut er = 1.0f64;
    for i in 1..8 {
        term *= r / f64::from(i);
        er += term;
    }
    (er * libm_exp2(k)) as f32
}

/// 2^k for an integer-valued `k` (built from the f64 exponent field).
fn libm_exp2(k: f64) -> f64 {
    let ki = k as i64;
    f64::from_bits((((ki + 1023) as u64) & 0x7ff) << 52)
}

/// v7.38 (read01, T12.1) — PG `word_distance`: how much a lexeme gap of `d`
/// positions dampens an AND cover's contribution. Only `calc_rank_and` uses it.
fn word_distance(d: u32) -> f32 {
    1.0 / (1.005 + 0.05 * exp_approx((d as f32) / 1.5 - 2.0))
}

/// A matched query-term occurrence: which query term, its position, its weight.
struct RankEntry {
    term: usize,
    pos: u16,
    w: f32,
}

/// Collect the distinct query-term words (in first-seen order).
fn collect_query_terms<'a>(query: &'a TsQueryAst, out: &mut Vec<&'a str>) {
    match query {
        TsQueryAst::Term { word, .. } => {
            if !out.iter().any(|t| *t == word.as_str()) {
                out.push(word.as_str());
            }
        }
        TsQueryAst::And(a, b) | TsQueryAst::Or(a, b) => {
            collect_query_terms(a, out);
            collect_query_terms(b, out);
        }
        TsQueryAst::Phrase { left, right, .. } => {
            collect_query_terms(left, out);
            collect_query_terms(right, out);
        }
        TsQueryAst::Not(_) => {}
    }
}

/// PG `calc_rank_or`: sum a per-term contribution over the term's positions,
/// then divide by the number of distinct query terms.
fn calc_rank_or(vec: &[TsLexeme], terms: &[&str]) -> f32 {
    let mut res = 0.0f32;
    for word in terms {
        if let Ok(idx) = vec.binary_search_by(|l| l.word.as_str().cmp(word)) {
            let wpos = weight_factor(vec[idx].weight);
            let (mut resj, mut wjm, mut jm) = (0.0f32, 0.0f32, 0usize);
            for (j, _pos) in vec[idx].positions.iter().enumerate() {
                let denom = ((j + 1) * (j + 1)) as f32;
                resj += wpos / denom;
                if wpos > wjm {
                    wjm = wpos;
                    jm = j;
                }
            }
            let jm_denom = ((jm + 1) * (jm + 1)) as f32;
            res += (wjm + resj - wjm / jm_denom) / 1.644_934_1;
        }
    }
    res / (terms.len().max(1) as f32)
}

/// PG `calc_rank_and`: probabilistic-OR combine of every position pair from
/// DISTINCT query terms, each weighted by the inter-position `word_distance`.
fn calc_rank_and(vec: &[TsLexeme], terms: &[&str]) -> f32 {
    let mut entries: Vec<RankEntry> = Vec::new();
    for (t, word) in terms.iter().enumerate() {
        if let Ok(idx) = vec.binary_search_by(|l| l.word.as_str().cmp(word)) {
            let w = weight_factor(vec[idx].weight);
            for &pos in &vec[idx].positions {
                entries.push(RankEntry { term: t, pos, w });
            }
        }
    }
    let mut res = -1.0f32;
    for i in 1..entries.len() {
        for k in 0..i {
            if entries[i].term == entries[k].term {
                continue;
            }
            let mut dist = u32::from(entries[i].pos.abs_diff(entries[k].pos));
            if dist == 0 {
                dist = 16384; // MAXENTRYPOS
            }
            let curw = sqrt_approx(entries[i].w * entries[k].w * word_distance(dist));
            res = if res < 0.0 {
                curw
            } else {
                1.0 - (1.0 - res) * (1.0 - curw)
            };
        }
    }
    if res < 0.0 {
        1e-20
    } else {
        res
    }
}

/// no_std `sqrt` for f32 (Newton, ample precision for ranking).
fn sqrt_approx(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    let mut g = f64::from(x);
    for _ in 0..20 {
        g = 0.5 * (g + f64::from(x) / g);
    }
    g as f32
}

fn collect_rank_terms(query: &TsQueryAst, vec: &[TsLexeme], score: &mut f32, n: &mut usize) {
    match query {
        TsQueryAst::Term { word, .. } => {
            *n += 1;
            if let Ok(idx) = vec.binary_search_by(|l| l.word.as_str().cmp(word.as_str())) {
                let w = vec[idx].weight;
                let occurrences = vec[idx].positions.len().max(1) as f32;
                *score += weight_factor(w) * occurrences;
            }
        }
        TsQueryAst::And(a, b) | TsQueryAst::Or(a, b) => {
            collect_rank_terms(a, vec, score, n);
            collect_rank_terms(b, vec, score, n);
        }
        TsQueryAst::Not(_) => {
            // NOT-side terms don't contribute to ts_rank.
        }
        TsQueryAst::Phrase { left, right, .. } => {
            collect_rank_terms(left, vec, score, n);
            collect_rank_terms(right, vec, score, n);
        }
    }
}

fn collect_cd_positions(
    query: &TsQueryAst,
    vec: &[TsLexeme],
    positions: &mut Vec<u16>,
    score: &mut f32,
    n: &mut usize,
) {
    match query {
        TsQueryAst::Term { word, .. } => {
            *n += 1;
            if let Ok(idx) = vec.binary_search_by(|l| l.word.as_str().cmp(word.as_str())) {
                positions.extend_from_slice(&vec[idx].positions);
                let w = vec[idx].weight;
                let occurrences = vec[idx].positions.len().max(1) as f32;
                *score += weight_factor(w) * occurrences;
            }
        }
        TsQueryAst::And(a, b) | TsQueryAst::Or(a, b) => {
            collect_cd_positions(a, vec, positions, score, n);
            collect_cd_positions(b, vec, positions, score, n);
        }
        TsQueryAst::Not(_) => {}
        TsQueryAst::Phrase { left, right, .. } => {
            collect_cd_positions(left, vec, positions, score, n);
            collect_cd_positions(right, vec, positions, score, n);
        }
    }
}

/// Tokenise on Unicode word boundaries — anything that is not an
/// alphanumeric scalar value (or `_`) splits the token. Lowercases
/// each emitted token.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() || c == '_' {
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

enum WebToken {
    Term(String),
    NotTerm(String),
    Phrase(Vec<String>),
    Or,
}

/// websearch tokenizer — splits on whitespace, recognises quoted
/// phrases, leading `-` for NOT, and bare `OR` (case-insensitive).
fn web_tokens(text: &str) -> Vec<WebToken> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b == b'"' {
            i += 1;
            let start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            let phrase_text = &text[start..i];
            let words: Vec<String> = phrase_text
                .split_whitespace()
                .map(|w| w.to_string())
                .collect();
            out.push(WebToken::Phrase(words));
            if i < bytes.len() {
                i += 1; // close quote
            }
            continue;
        }
        let negate = b == b'-';
        if negate {
            i += 1;
        }
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'"' {
            i += 1;
        }
        let word = &text[start..i];
        if word.eq_ignore_ascii_case("or") && !negate {
            out.push(WebToken::Or);
        } else if negate {
            out.push(WebToken::NotTerm(word.to_string()));
        } else {
            out.push(WebToken::Term(word.to_string()));
        }
    }
    out
}

/// PG's standard english stopword list (`tsearch_data/english.stop`).
/// Subset of the 127 words in PG 17's distribution — verbatim.
pub fn is_english_stopword(word: &str) -> bool {
    matches!(
        word,
        "i" | "me"
            | "my"
            | "myself"
            | "we"
            | "our"
            | "ours"
            | "ourselves"
            | "you"
            | "your"
            | "yours"
            | "yourself"
            | "yourselves"
            | "he"
            | "him"
            | "his"
            | "himself"
            | "she"
            | "her"
            | "hers"
            | "herself"
            | "it"
            | "its"
            | "itself"
            | "they"
            | "them"
            | "their"
            | "theirs"
            | "themselves"
            | "what"
            | "which"
            | "who"
            | "whom"
            | "this"
            | "that"
            | "these"
            | "those"
            | "am"
            | "is"
            | "are"
            | "was"
            | "were"
            | "be"
            | "been"
            | "being"
            | "have"
            | "has"
            | "had"
            | "having"
            | "do"
            | "does"
            | "did"
            | "doing"
            | "a"
            | "an"
            | "the"
            | "and"
            | "but"
            | "if"
            | "or"
            | "because"
            | "as"
            | "until"
            | "while"
            | "of"
            | "at"
            | "by"
            | "for"
            | "with"
            | "about"
            | "against"
            | "between"
            | "into"
            | "through"
            | "during"
            | "before"
            | "after"
            | "above"
            | "below"
            | "to"
            | "from"
            | "up"
            | "down"
            | "in"
            | "out"
            | "on"
            | "off"
            | "over"
            | "under"
            | "again"
            | "further"
            | "then"
            | "once"
            | "here"
            | "there"
            | "when"
            | "where"
            | "why"
            | "how"
            | "all"
            | "any"
            | "both"
            | "each"
            | "few"
            | "more"
            | "most"
            | "other"
            | "some"
            | "such"
            | "no"
            | "nor"
            | "not"
            | "only"
            | "own"
            | "same"
            | "so"
            | "than"
            | "too"
            | "very"
            | "s"
            | "t"
            | "can"
            | "will"
            | "just"
            | "don"
            | "should"
            | "now"
    )
}

// --------------------------------------------------------------
// Porter stemmer (English, original 1980 algorithm). Operates on
// pure ASCII — non-ASCII input falls through unchanged.
// --------------------------------------------------------------

/// v7.12.1 — Porter v1 stem. Lowercased ASCII input gives a
/// stemmed form; non-ASCII characters bypass the algorithm
/// (returned verbatim).
pub fn porter_stem(word: &str) -> String {
    if !word.is_ascii() {
        return word.to_string();
    }
    let bytes: Vec<u8> = word.bytes().collect();
    if bytes.len() <= 2 {
        return word.to_string();
    }
    let mut b = bytes;
    step1a(&mut b);
    step1b(&mut b);
    step1c(&mut b);
    step2(&mut b);
    step3(&mut b);
    step4(&mut b);
    step5a(&mut b);
    step5b(&mut b);
    // Safe: we only ever produced ASCII via the steps above.
    String::from_utf8(b).expect("porter stem produced non-UTF8 bytes")
}

fn is_vowel(b: &[u8], i: usize) -> bool {
    match b[i] {
        b'a' | b'e' | b'i' | b'o' | b'u' => true,
        b'y' => i > 0 && !is_vowel(b, i - 1),
        _ => false,
    }
}

/// Porter's `m` measure — the number of `[C](VC)^m[V]` units.
fn measure(b: &[u8]) -> usize {
    let mut m = 0;
    let mut prev_vowel = false;
    let mut started = false;
    for i in 0..b.len() {
        let v = is_vowel(b, i);
        if started && prev_vowel && !v {
            m += 1;
        }
        prev_vowel = v;
        started = true;
    }
    m
}

fn has_vowel(b: &[u8]) -> bool {
    (0..b.len()).any(|i| is_vowel(b, i))
}

fn ends_with(b: &[u8], suf: &[u8]) -> bool {
    b.len() >= suf.len() && &b[b.len() - suf.len()..] == suf
}

fn replace_suffix(b: &mut Vec<u8>, suf_len: usize, new_suf: &[u8]) {
    let new_len = b.len() - suf_len;
    b.truncate(new_len);
    b.extend_from_slice(new_suf);
}

fn measure_stem(b: &[u8], suf_len: usize) -> usize {
    measure(&b[..b.len() - suf_len])
}

fn step1a(b: &mut Vec<u8>) {
    if ends_with(b, b"sses") {
        replace_suffix(b, 4, b"ss");
    } else if ends_with(b, b"ies") {
        replace_suffix(b, 3, b"i");
    } else if ends_with(b, b"ss") {
        // No change.
    } else if ends_with(b, b"s") {
        replace_suffix(b, 1, b"");
    }
}

fn step1b_post(b: &mut Vec<u8>) {
    if ends_with(b, b"at") {
        replace_suffix(b, 2, b"ate");
    } else if ends_with(b, b"bl") {
        replace_suffix(b, 2, b"ble");
    } else if ends_with(b, b"iz") {
        replace_suffix(b, 2, b"ize");
    } else if b.len() >= 2 && b[b.len() - 1] == b[b.len() - 2] {
        let last = b[b.len() - 1];
        if !matches!(last, b'l' | b's' | b'z') {
            b.pop();
        }
    } else if cvc(b) {
        b.extend_from_slice(b"e");
    }
}

fn cvc(b: &[u8]) -> bool {
    if b.len() < 3 {
        return false;
    }
    let l = b.len();
    if !(is_vowel(b, l - 2) && !is_vowel(b, l - 3) && !is_vowel(b, l - 1)) {
        return false;
    }
    !matches!(b[l - 1], b'w' | b'x' | b'y')
}

fn step1b(b: &mut Vec<u8>) {
    if ends_with(b, b"eed") {
        if measure_stem(b, 3) > 0 {
            replace_suffix(b, 3, b"ee");
        }
        return;
    }
    if ends_with(b, b"ed") {
        let stem_has_vowel = has_vowel(&b[..b.len() - 2]);
        if stem_has_vowel {
            replace_suffix(b, 2, b"");
            step1b_post(b);
        }
        return;
    }
    if ends_with(b, b"ing") {
        let stem_has_vowel = has_vowel(&b[..b.len() - 3]);
        if stem_has_vowel {
            replace_suffix(b, 3, b"");
            step1b_post(b);
        }
    }
}

fn step1c(b: &mut Vec<u8>) {
    if ends_with(b, b"y") && has_vowel(&b[..b.len() - 1]) {
        replace_suffix(b, 1, b"i");
    }
}

const STEP2_RULES: &[(&[u8], &[u8])] = &[
    (b"ational", b"ate"),
    (b"tional", b"tion"),
    (b"enci", b"ence"),
    (b"anci", b"ance"),
    (b"izer", b"ize"),
    (b"abli", b"able"),
    (b"alli", b"al"),
    (b"entli", b"ent"),
    (b"eli", b"e"),
    (b"ousli", b"ous"),
    (b"ization", b"ize"),
    (b"ation", b"ate"),
    (b"ator", b"ate"),
    (b"alism", b"al"),
    (b"iveness", b"ive"),
    (b"fulness", b"ful"),
    (b"ousness", b"ous"),
    (b"aliti", b"al"),
    (b"iviti", b"ive"),
    (b"biliti", b"ble"),
];

fn step2(b: &mut Vec<u8>) {
    for (suf, repl) in STEP2_RULES {
        if ends_with(b, suf) && measure_stem(b, suf.len()) > 0 {
            replace_suffix(b, suf.len(), repl);
            return;
        }
    }
}

const STEP3_RULES: &[(&[u8], &[u8])] = &[
    (b"icate", b"ic"),
    (b"ative", b""),
    (b"alize", b"al"),
    (b"iciti", b"ic"),
    (b"ical", b"ic"),
    (b"ful", b""),
    (b"ness", b""),
];

fn step3(b: &mut Vec<u8>) {
    for (suf, repl) in STEP3_RULES {
        if ends_with(b, suf) && measure_stem(b, suf.len()) > 0 {
            replace_suffix(b, suf.len(), repl);
            return;
        }
    }
}

const STEP4_RULES: &[&[u8]] = &[
    b"al", b"ance", b"ence", b"er", b"ic", b"able", b"ible", b"ant", b"ement", b"ment", b"ent",
    b"ou", b"ism", b"ate", b"iti", b"ous", b"ive", b"ize",
];

fn step4(b: &mut Vec<u8>) {
    // Special-case `ion` — only strip when preceded by s/t.
    if ends_with(b, b"ion") && measure_stem(b, 3) > 1 {
        let stem = &b[..b.len() - 3];
        if matches!(stem.last(), Some(b's') | Some(b't')) {
            replace_suffix(b, 3, b"");
            return;
        }
    }
    for suf in STEP4_RULES {
        if ends_with(b, suf) && measure_stem(b, suf.len()) > 1 {
            replace_suffix(b, suf.len(), b"");
            return;
        }
    }
}

fn step5a(b: &mut Vec<u8>) {
    if ends_with(b, b"e") {
        let m = measure_stem(b, 1);
        if m > 1 || (m == 1 && !cvc(&b[..b.len() - 1])) {
            replace_suffix(b, 1, b"");
        }
    }
}

fn step5b(b: &mut Vec<u8>) {
    if b.len() >= 2 && b[b.len() - 1] == b'l' && b[b.len() - 2] == b'l' && measure(b) > 1 {
        b.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porter_simple_cases() {
        assert_eq!(porter_stem("caresses"), "caress");
        assert_eq!(porter_stem("ponies"), "poni");
        assert_eq!(porter_stem("ties"), "ti");
        assert_eq!(porter_stem("cats"), "cat");
        assert_eq!(porter_stem("running"), "run");
        assert_eq!(porter_stem("happy"), "happi");
        assert_eq!(porter_stem("relational"), "relat");
        assert_eq!(porter_stem("conditional"), "condit");
        assert_eq!(porter_stem("hopefulness"), "hope");
    }

    #[test]
    fn english_drops_stopwords_and_stems() {
        let v = to_tsvector(
            TsConfig::English,
            "The quick brown foxes are jumping over the lazy dogs",
        );
        let words: Vec<&str> = v.iter().map(|l| l.word.as_str()).collect();
        // Stopwords removed: the, are, over
        // Stems: quick → quick, brown → brown, foxes → fox,
        // jumping → jump, lazy → lazi, dogs → dog.
        assert!(words.contains(&"fox"), "expected `fox`, got {words:?}");
        assert!(words.contains(&"jump"), "expected `jump`, got {words:?}");
        assert!(words.contains(&"dog"), "expected `dog`, got {words:?}");
        assert!(!words.contains(&"the"), "stopword `the` leaked: {words:?}");
        assert!(!words.contains(&"are"), "stopword `are` leaked: {words:?}");
    }

    #[test]
    fn simple_preserves_words() {
        let v = to_tsvector(TsConfig::Simple, "The Quick brown Foxes");
        let words: Vec<&str> = v.iter().map(|l| l.word.as_str()).collect();
        // Sorted ascending.
        assert_eq!(words, alloc::vec!["brown", "foxes", "quick", "the"]);
    }

    #[test]
    fn plainto_tsquery_drops_stopwords() {
        let q = plainto_tsquery(TsConfig::English, "the quick brown fox");
        // Expect (quick & brown) & fox after stopword drop.
        let s = crate::eval::format_tsquery(&q);
        assert_eq!(s, "'quick' & 'brown' & 'fox'");
    }

    #[test]
    fn to_tsquery_stems_terms() {
        let q = to_tsquery(TsConfig::English, "running & jumps").unwrap();
        let s = crate::eval::format_tsquery(&q);
        assert_eq!(s, "'run' & 'jump'");
    }
}
