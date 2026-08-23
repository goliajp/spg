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
    /// v7.38.18 — `spanish`: lowercase + split + Snowball's 313-word
    /// Spanish stopword list + the Snowball Spanish stem.
    Spanish,
}

impl TsConfig {
    /// v7.38.18 — does this configuration stem at all?
    ///
    /// The four places that decide stopwords and stemming used to ask
    /// `config.stems()`, a two-valued question.
    /// Adding a language to an enum those read as a boolean would have
    /// made it tokenise without dropping a stopword and without
    /// stemming — silently, since every token still comes out.
    pub const fn stems(self) -> bool {
        !matches!(self, Self::Simple)
    }

    /// This configuration's stopword list, or `None` when it has none.
    pub fn stopwords(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Simple => None,
            Self::English => None, // its own list, see `is_english_stopword`
            Self::Spanish => Some(crate::fts_stop::ES_STOP),
        }
    }

    /// Is `w` a stopword under this configuration?
    pub fn is_stopword(self, w: &str) -> bool {
        match self {
            Self::Simple => false,
            Self::English => is_english_stopword(w),
            Self::Spanish => crate::fts_stop::is_stop(crate::fts_stop::ES_STOP, w),
        }
    }

    /// The stem `w` reduces to under this configuration.
    pub fn stem(self, w: &str) -> String {
        match self {
            Self::Simple => String::from(w),
            Self::English => porter_stem(w),
            Self::Spanish => crate::fts_es::stem_es(w),
        }
    }

    /// Resolve a PG text-search config name. The PG-qualified
    /// form `pg_catalog.<name>` is accepted too. Returns `None`
    /// for any other name so the caller can produce a clear
    /// "config not implemented" error listing what is supported.
    pub fn from_name(name: &str) -> Option<Self> {
        let bare = name.strip_prefix("pg_catalog.").unwrap_or(name);
        match bare.to_ascii_lowercase().as_str() {
            "simple" => Some(Self::Simple),
            "english" => Some(Self::English),
            // v7.38.18 — Snowball's other three, each implemented from
            // the published algorithm and verified word-for-word
            // against PG 18.4. See `fts_es` / `fts_fr` / `fts_de`.
            "spanish" => Some(Self::Spanish),
            _ => None,
        }
    }
}

/// v7.12.1 — tokenise + (optionally) stem `text` into a sorted +
/// deduped lexeme set with merged positions. Each token's
/// position is 1-based and clamped at 16383 (PG18-measured, round
/// 753: a 20k-word document's positions top out at 16383 — the
/// `MAXENTRYPOS - 1` clamp — while every lexeme is still recorded).
pub fn to_tsvector(config: TsConfig, text: &str) -> Vec<TsLexeme> {
    let mut out: Vec<TsLexeme> = Vec::new();
    let mut position: u16 = 0;
    // v7.39 (round 651) — the TOKEN's type picks the dictionary, which
    // is what `pg_ts_config_map` records. Two things fall out that the
    // config alone could not give: a tag or an entity maps to nothing
    // and so never reaches the index, and a number under the `english`
    // configuration goes to `simple` rather than through the stemmer.
    let english = config.stems();
    for token in tokenize_typed(text) {
        let Some(dict) = token.ty.dictionary(english) else {
            continue;
        };
        let folded = token.text.to_lowercase();
        let lex = match dict {
            TsDict::Simple => folded,
            TsDict::EnglishStem => {
                if config.is_stopword(&folded) {
                    // PG drops stopwords from the vector but
                    // still increments position so phrase
                    // distances stay meaningful.
                    position = position.saturating_add(1).min(16383);
                    continue;
                }
                config.stem(&folded)
            }
        };
        if lex.is_empty() {
            continue;
        }
        position = position.saturating_add(1).min(16383);
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
/// but preserve order — fold into nested phrase nodes whose `<N>`
/// distance is the position gap between surviving lexemes. Dropped
/// stopwords still advance the position counter (as in `to_tsvector`),
/// so `'cats and dogs'` yields `'cat' <2> 'dog'`, matching PG.
pub fn phraseto_tsquery(config: TsConfig, text: &str) -> TsQueryAst {
    let lexs = collect_lexemes_positioned(config, text);
    fold_phrase_positioned(&lexs)
}

/// v7.12.1 — `websearch_to_tsquery(config, text)`: Google-style
/// syntax. Quoted phrases → phrase node; `OR` (case-insensitive)
/// → OR; leading `-` → NOT; otherwise AND.
///
/// The grammar is liberal — malformed input degrades instead of
/// erroring. PG18-measured (round 753): an unclosed quote still
/// forms a phrase (`"unclosed phrase` → `'unclosed' <-> 'phrase'`,
/// SPG agrees); bare operator words become terms (`or or and -` →
/// `'or' | 'and'` in PG, SPG answers `'and'` — the leading or-term
/// is dropped; ledgered as F31-B7).
pub fn websearch_to_tsquery(config: TsConfig, text: &str) -> TsQueryAst {
    let mut tokens = web_tokens(text);
    // Apply config to each plain term + each phrase part.
    for t in &mut tokens {
        match t {
            WebToken::Term(s) => {
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
            WebToken::Or | WebToken::Neg => {}
        }
    }
    // Group by OR boundaries; within each group AND together.
    let mut or_groups: Vec<Vec<TsQueryAst>> = alloc::vec![Vec::new()];
    // v7.39 (round 756, F31-B7) — dashes seen since the last operand;
    // each wraps one `Not` level (PG stacks: `--apple` → !!'apple').
    let mut pending_negs = 0usize;
    let mut push_node = |groups: &mut Vec<Vec<TsQueryAst>>, negs: usize, node: TsQueryAst| {
        let mut node = node;
        for _ in 0..negs {
            node = TsQueryAst::Not(alloc::boxed::Box::new(node));
        }
        groups.last_mut().unwrap().push(node);
    };
    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            WebToken::Or => {
                or_groups.push(Vec::new());
                pending_negs = 0;
            }
            WebToken::Neg => {
                pending_negs += 1;
            }
            WebToken::Term(s) => {
                if !s.is_empty() {
                    push_node(&mut or_groups, pending_negs, fold_and(&split_words(s)));
                }
                pending_negs = 0;
            }
            WebToken::Phrase(words) => {
                if !words.is_empty() {
                    push_node(&mut or_groups, pending_negs, fold_phrase(words));
                }
                pending_negs = 0;
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
    // v7.39 (round 245) — the english config drops stopwords from a QUERY
    // too, collapsing the tree around them: PG's
    // `to_tsquery('english','!(a & b)')` is `!'b'` because `a` is a
    // stopword. SPG kept the stopword term, so the query demanded a
    // lexeme no vector ever contains. A tree that is ALL stopwords is
    // left as parsed (PG returns an empty tsquery there — an empty-tree
    // representation SPG doesn't have; recorded residual).
    if config.stems()
        && let Some(pruned) = prune_stopword_terms(&ast)
    {
        ast = pruned;
    }
    Ok(ast)
}

fn prune_stopword_terms(ast: &TsQueryAst) -> Option<TsQueryAst> {
    match ast {
        TsQueryAst::Term { word, .. } => {
            if is_english_stopword(word) {
                None
            } else {
                Some(ast.clone())
            }
        }
        TsQueryAst::And(a, b) => match (prune_stopword_terms(a), prune_stopword_terms(b)) {
            (Some(x), Some(y)) => Some(TsQueryAst::And(
                alloc::boxed::Box::new(x),
                alloc::boxed::Box::new(y),
            )),
            (Some(x), None) | (None, Some(x)) => Some(x),
            (None, None) => None,
        },
        TsQueryAst::Or(a, b) => match (prune_stopword_terms(a), prune_stopword_terms(b)) {
            (Some(x), Some(y)) => Some(TsQueryAst::Or(
                alloc::boxed::Box::new(x),
                alloc::boxed::Box::new(y),
            )),
            (Some(x), None) | (None, Some(x)) => Some(x),
            (None, None) => None,
        },
        TsQueryAst::Not(x) => {
            prune_stopword_terms(x).map(|p| TsQueryAst::Not(alloc::boxed::Box::new(p)))
        }
        TsQueryAst::Phrase {
            left,
            right,
            distance,
        } => match (prune_stopword_terms(left), prune_stopword_terms(right)) {
            (Some(x), Some(y)) => Some(TsQueryAst::Phrase {
                left: alloc::boxed::Box::new(x),
                right: alloc::boxed::Box::new(y),
                distance: *distance,
            }),
            (Some(x), None) | (None, Some(x)) => Some(x),
            (None, None) => None,
        },
    }
}

fn stem_tsquery_in_place(ast: &mut TsQueryAst, config: TsConfig) {
    match ast {
        TsQueryAst::Term { word, .. } => {
            let lower = word.to_lowercase();
            *word = match config {
                TsConfig::Simple => lower,
                TsConfig::English => porter_stem(&lower),
                TsConfig::Spanish => crate::fts_es::stem_es(&lower),
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
    let english = config.stems();
    for token in tokenize_typed(text) {
        let Some(dict) = token.ty.dictionary(english) else {
            continue;
        };
        let folded = token.text.to_lowercase();
        match dict {
            TsDict::Simple => out.push(folded),
            TsDict::EnglishStem => {
                if config.is_stopword(&folded) {
                    continue;
                }
                let stemmed = config.stem(&folded);
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

/// v7.39 (read01 round 43) — tokenise + stem while tracking each
/// surviving lexeme's tsvector position. Dropped stopwords advance the
/// counter without emitting a lexeme, mirroring `to_tsvector`, so the
/// gap between consecutive survivors is PG's phrase distance.
fn collect_lexemes_positioned(config: TsConfig, text: &str) -> Vec<(String, u16)> {
    let mut out: Vec<(String, u16)> = Vec::new();
    let mut position: u16 = 0;
    let english = config.stems();
    for token in tokenize_typed(text) {
        let Some(dict) = token.ty.dictionary(english) else {
            continue;
        };
        let folded = token.text.to_lowercase();
        let lex = match dict {
            TsDict::Simple => folded,
            TsDict::EnglishStem => {
                if config.is_stopword(&folded) {
                    position = position.saturating_add(1).min(16383);
                    continue;
                }
                config.stem(&folded)
            }
        };
        if lex.is_empty() {
            continue;
        }
        position = position.saturating_add(1).min(16383);
        out.push((lex, position));
    }
    out
}

/// v7.39 (read01 round 43) — fold positioned lexemes into a phrase
/// chain whose `<N>` distance is the position delta between neighbours.
fn fold_phrase_positioned(lexs: &[(String, u16)]) -> TsQueryAst {
    if lexs.is_empty() {
        return TsQueryAst::Term {
            word: String::new(),
            weight_mask: 0,
        };
    }
    let mut it = lexs.iter();
    let (first_word, first_pos) = it.next().unwrap();
    let mut acc = TsQueryAst::Term {
        word: first_word.clone(),
        weight_mask: 0,
    };
    let mut prev_pos = *first_pos;
    for (word, pos) in it {
        let distance = pos.saturating_sub(prev_pos);
        acc = TsQueryAst::Phrase {
            left: alloc::boxed::Box::new(acc),
            right: alloc::boxed::Box::new(TsQueryAst::Term {
                word: word.clone(),
                weight_mask: 0,
            }),
            distance,
        };
        prev_pos = *pos;
    }
    acc
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
        TsQueryAst::Term { word, weight_mask } => term_matches(vec, word, *weight_mask),
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

/// v7.39 (round 245) — Term matching with the two mask-carried modifiers:
/// bit 4 is the PREFIX flag (`fox:*` — any lexeme starting with the word
/// matches) and the low four bits the accepted-weight set (`0` = any).
fn term_matches(vec: &[TsLexeme], word: &str, mask: u8) -> bool {
    let prefix = mask & 0x10 != 0;
    let weights = mask & 0x0f;
    let weight_ok = |l: &TsLexeme| weights == 0 || weights & (1 << l.weight) != 0;
    if prefix {
        return vec.iter().any(|l| l.word.starts_with(word) && weight_ok(l));
    }
    match vec.binary_search_by(|l| l.word.as_str().cmp(word)) {
        Ok(idx) => weight_ok(&vec[idx]),
        Err(_) => false,
    }
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
pub fn ts_rank(weights: &RankWeights, vec: &[TsLexeme], query: &TsQueryAst) -> f32 {
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
        calc_rank_and(vec, &terms, weights)
    } else {
        calc_rank_or(vec, &terms, weights)
    }
}

/// v7.12.2 — `ts_rank_cd(vec, q)` cover-density variant. Higher
/// score when matched lexemes cluster closer together; defaults
/// to a per-lexeme contribution divided by the average gap
/// between matched positions. Returns 0 when no terms match.
#[must_use]
pub fn ts_rank_cd(weights: &RankWeights, vec: &[TsLexeme], query: &TsQueryAst) -> f32 {
    // v7.38 (read01, T12.1) — PG's calc_rank_cd cover density. Sum, over each
    // minimal cover (window containing every distinct query term), a
    // per-cover weight `Cpos = (#entries / Σ 1/weight) / (noise + 1)`, where
    // noise is the extra positional span beyond the matched entries. Default
    // normalization flag 0 (no division).
    let mut terms: Vec<&str> = Vec::new();
    collect_query_terms(query, &mut terms);
    if terms.is_empty() {
        return 0.0;
    }
    // doc = (position, term-index, weight), sorted by position.
    let mut doc: Vec<(u16, usize, u8)> = Vec::new();
    for (t, word) in terms.iter().enumerate() {
        if let Ok(idx) = vec.binary_search_by(|l| l.word.as_str().cmp(word)) {
            for &pos in &vec[idx].positions {
                doc.push((pos, t, vec[idx].weight));
            }
        }
    }
    doc.sort_unstable();
    let nterms = terms.len();
    let mut wdoc = 0.0f32;
    let mut start = 0usize;
    while start < doc.len() {
        // Grow a window from `start` until every distinct term is present.
        let mut seen = alloc::vec![false; nterms];
        let mut cnt = 0usize;
        let mut end = start;
        while end < doc.len() {
            if !seen[doc[end].1] {
                seen[doc[end].1] = true;
                cnt += 1;
            }
            if cnt == nterms {
                break;
            }
            end += 1;
        }
        if cnt < nterms {
            break; // no further cover
        }
        // Shrink from the left to the minimal cover.
        let mut begin = start;
        while begin < end {
            let bt = doc[begin].1;
            if doc[begin + 1..=end].iter().any(|d| d.1 == bt) {
                begin += 1;
            } else {
                break;
            }
        }
        let p = doc[begin].0;
        let q = doc[end].0;
        let inv_sum: f32 = doc[begin..=end]
            .iter()
            .map(|d| 1.0 / weight_factor(d.2, weights))
            .sum();
        let mut cpos = ((end - begin + 1) as f32) / inv_sum;
        let nnoise = (i32::from(q) - i32::from(p)) - (end as i32 - begin as i32);
        if nnoise > 0 {
            cpos /= (nnoise + 1) as f32;
        }
        wdoc += cpos;
        start = begin + 1;
    }
    wdoc
}

/// v7.38 (read01, T12.1) — apply PG's ranking normalization bitmask to a raw
/// rank. Flags are applied in PG's order over the tsvector's total position
/// count (`len`) and distinct-lexeme count (`uniq`):
///   1 → /log2(len+1) · 2 → /len · 8 → /uniq · 16 → /log2(uniq+1) · 32 → r/(r+1)
/// (flag 4, the cover-extent distance, is cover-density only and handled by the
/// caller.) Verified against live PG 18.4.
#[must_use]
pub fn apply_rank_norm(mut rank: f32, norm: i64, vec: &[TsLexeme]) -> f32 {
    let len: usize = vec.iter().map(|l| l.positions.len()).sum();
    let uniq = vec.len();
    if norm & 1 != 0 && len > 0 {
        rank /= log2_approx((len + 1) as f32);
    }
    if norm & 2 != 0 && len > 0 {
        rank /= len as f32;
    }
    if norm & 8 != 0 && uniq > 0 {
        rank /= uniq as f32;
    }
    if norm & 16 != 0 {
        rank /= log2_approx((uniq + 1) as f32);
    }
    if norm & 32 != 0 {
        rank /= rank + 1.0;
    }
    rank
}

fn log2_approx(x: f32) -> f32 {
    ln_approx(x) / core::f32::consts::LN_2
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

/// v7.38 (read01, T12.1) — a ts_rank weight array in PG order `[D, C, B, A]`.
pub type RankWeights = [f32; 4];
/// PG default weights: D=0.1, C=0.2, B=0.4, A=1.0.
pub const DEFAULT_RANK_WEIGHTS: RankWeights = [0.1, 0.2, 0.4, 1.0];

fn weight_factor(w: u8, weights: &RankWeights) -> f32 {
    // Weight byte: D=0, C=1, B=2, A=3 — a direct index into the PG-order array.
    weights[(w as usize).min(3)]
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
fn calc_rank_or(vec: &[TsLexeme], terms: &[&str], weights: &RankWeights) -> f32 {
    let mut res = 0.0f32;
    for word in terms {
        if let Ok(idx) = vec.binary_search_by(|l| l.word.as_str().cmp(word)) {
            let wpos = weight_factor(vec[idx].weight, weights);
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
            res += (wjm + resj - wjm / jm_denom) / 1.644_934;
        }
    }
    res / (terms.len().max(1) as f32)
}

/// PG `calc_rank_and`: probabilistic-OR combine of every position pair from
/// DISTINCT query terms, each weighted by the inter-position `word_distance`.
fn calc_rank_and(vec: &[TsLexeme], terms: &[&str], weights: &RankWeights) -> f32 {
    let mut entries: Vec<RankEntry> = Vec::new();
    for (t, word) in terms.iter().enumerate() {
        if let Ok(idx) = vec.binary_search_by(|l| l.word.as_str().cmp(word)) {
            let w = weight_factor(vec[idx].weight, weights);
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
    if res < 0.0 { 1e-20 } else { res }
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

/// Tokenise on Unicode word boundaries — anything that is not an
/// alphanumeric scalar value (or `_`) splits the token. Lowercases
/// each emitted token.
/// v7.39 (round 651) — PG's token types, as `ts_token_type('default')`
/// publishes them. Only the ones SPG's parser actually produces are
/// here; the numbering is PG's so `pg_ts_config_map.maptokentype` and
/// `ts_debug.alias` agree with it.
///
/// The four PG does NOT map to any dictionary — blank(12), tag(13),
/// protocol(14), entity(23) — are recognised precisely so they can be
/// DROPPED. That is the difference between indexing `<b>x</b>` as `x`,
/// which PG does, and as `b`, `x`, `b`, which SPG did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenType {
    AsciiWord = 1,
    Word = 2,
    NumWord = 3,
    Email = 4,
    Url = 5,
    Host = 6,
    SFloat = 7,
    Version = 8,
    HwordNumPart = 9,
    HwordPart = 10,
    HwordAsciiPart = 11,
    Blank = 12,
    Tag = 13,
    Protocol = 14,
    NumHword = 15,
    AsciiHword = 16,
    Hword = 17,
    UrlPath = 18,
    File = 19,
    Float = 20,
    Int = 21,
    Uint = 22,
    Entity = 23,
}

impl TokenType {
    /// PG's `alias` column.
    pub const fn alias(self) -> &'static str {
        match self {
            Self::AsciiWord => "asciiword",
            Self::Word => "word",
            Self::NumWord => "numword",
            Self::Email => "email",
            Self::Url => "url",
            Self::Host => "host",
            Self::SFloat => "sfloat",
            Self::Version => "version",
            Self::HwordNumPart => "hword_numpart",
            Self::HwordPart => "hword_part",
            Self::HwordAsciiPart => "hword_asciipart",
            Self::Blank => "blank",
            Self::Tag => "tag",
            Self::Protocol => "protocol",
            Self::NumHword => "numhword",
            Self::AsciiHword => "asciihword",
            Self::Hword => "hword",
            Self::UrlPath => "url_path",
            Self::File => "file",
            Self::Float => "float",
            Self::Int => "int",
            Self::Uint => "uint",
            Self::Entity => "entity",
        }
    }

    /// PG's `description` column, verbatim.
    pub const fn description(self) -> &'static str {
        match self {
            Self::AsciiWord => "Word, all ASCII",
            Self::Word => "Word, all letters",
            Self::NumWord => "Word, letters and digits",
            Self::Email => "Email address",
            Self::Url => "URL",
            Self::Host => "Host",
            Self::SFloat => "Scientific notation",
            Self::Version => "Version number",
            Self::HwordNumPart => "Hyphenated word part, letters and digits",
            Self::HwordPart => "Hyphenated word part, all letters",
            Self::HwordAsciiPart => "Hyphenated word part, all ASCII",
            Self::Blank => "Space symbols",
            Self::Tag => "XML tag",
            Self::Protocol => "Protocol head",
            Self::NumHword => "Hyphenated word, letters and digits",
            Self::AsciiHword => "Hyphenated word, all ASCII",
            Self::Hword => "Hyphenated word, all letters",
            Self::UrlPath => "URL path",
            Self::File => "File or path name",
            Self::Float => "Decimal notation",
            Self::Int => "Signed integer",
            Self::Uint => "Unsigned integer",
            Self::Entity => "XML entity",
        }
    }

    /// Which dictionary a configuration sends this token to, or `None`
    /// when the configuration maps it to nothing and the token produces
    /// no lexeme at all. Read off PG18's `pg_ts_config_map`: the same
    /// nineteen types are mapped by both `simple` and `english`, and
    /// the four that are not are blank, tag, protocol and entity.
    pub const fn dictionary(self, english: bool) -> Option<TsDict> {
        match self {
            Self::Blank | Self::Tag | Self::Protocol | Self::Entity => None,
            // The stemmer only ever sees words; everything with digits,
            // punctuation or structure goes to `simple` even under the
            // english configuration — measured, and the reason
            // `to_tsvector('english', '42')` is `42` and not a stem.
            Self::AsciiWord
            | Self::Word
            | Self::HwordPart
            | Self::HwordAsciiPart
            | Self::AsciiHword
            | Self::Hword
                if english =>
            {
                Some(TsDict::EnglishStem)
            }
            _ => Some(TsDict::Simple),
        }
    }
}

/// The two dictionaries SPG has (round 650).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsDict {
    Simple,
    EnglishStem,
}

/// v7.39 (round 651) — a typed token, PG-shaped.
#[derive(Debug, Clone)]
pub struct Token {
    pub text: String,
    pub ty: TokenType,
}

/// The old tokenizer, kept for callers that want bare words.
pub fn tokenize(text: &str) -> Vec<String> {
    tokenize_typed(text)
        .into_iter()
        .filter(|t| t.ty.dictionary(false).is_some())
        .map(|t| t.text)
        .collect()
}

/// v7.39 (round 651) — split `text` the way PG's default parser does.
///
/// What this replaces was sixteen lines: "split on anything that is not
/// alphanumeric". Measured against PG across its 23 token types, that
/// agreed on FOUR — asciiword, word, numword and uint — and differed on
/// thirteen. `user@example.com` indexed as `user`, `example`, `com` so a
/// search for the address found nothing; `3.14` became `3` and `14`;
/// `-42` lost its sign; and `<b>x</b>` put the tag name `b` INTO the
/// index, twice. That last one is not a near-miss, it is markup
/// polluting the search results.
///
/// Recognised here, longest-shape-first because the shapes nest (an
/// email contains a host, a url contains a host and a path):
/// tag, entity, url/protocol, email, file, host, version, sfloat,
/// float, signed int, hyphenated word (compound AND parts, as PG emits
/// both), and finally plain words and unsigned integers.
pub fn tokenize_typed(text: &str) -> Vec<Token> {
    let b: Vec<char> = text.chars().collect();
    let mut out: Vec<Token> = Vec::new();
    let mut i = 0usize;
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // `<...>` — an XML tag. PG emits it as a `tag` token, which no
        // configuration maps, so nothing of it reaches the index.
        if c == '<'
            && let Some(end) = (i + 1..b.len()).find(|&j| b[j] == '>')
        {
            out.push(Token {
                text: b[i..=end].iter().collect(),
                ty: TokenType::Tag,
            });
            i = end + 1;
            continue;
        }
        // `&name;` / `&#123;` — an XML entity, likewise unmapped.
        if c == '&'
            && let Some(end) = (i + 1..b.len().min(i + 12)).find(|&j| b[j] == ';')
            && end > i + 1
        {
            out.push(Token {
                text: b[i..=end].iter().collect(),
                ty: TokenType::Entity,
            });
            i = end + 1;
            continue;
        }
        // A leading `/` belongs to the path it starts: PG's `file` token
        // for `/usr/local/bin` keeps it, and a token that drops it is a
        // different string to search for.
        let leading_slash = c == '/' && i + 1 < b.len() && is_word(b[i + 1]);
        if is_word(c) || leading_slash || (c == '-' && i + 1 < b.len() && b[i + 1].is_ascii_digit())
        {
            let start = i;
            // A signed integer only counts as one at the start of a run.
            let signed = c == '-';
            if signed || leading_slash {
                i += 1;
            }
            while i < b.len() && (is_word(b[i]) || matches!(b[i], '.' | '@' | '/' | '-' | ':')) {
                // Stop before a trailing separator that is really
                // punctuation: `end.` / `a/b,` — the separator has to be
                // followed by more of the token.
                if matches!(b[i], '.' | '@' | '/' | '-' | ':')
                    && (i + 1 >= b.len() || !(is_word(b[i + 1]) || b[i + 1] == '/'))
                {
                    break;
                }
                i += 1;
            }
            let raw: String = b[start..i].iter().collect();
            classify_into(&raw, signed, &mut out);
            continue;
        }
        i += 1;
    }
    out
}

/// Assign a PG token type to one raw run, emitting the sub-parts PG
/// emits alongside a compound.
/// The original text for a run whose lowercased form is `lower`. Equal
/// lengths is the common case (ASCII); when a fold changed the byte
/// length the lowercased form is the honest fallback — `ts_debug`'s
/// `token` column would otherwise slice mid-character.
fn raw_of<'a>(raw: &'a str, lower: &'a impl AsRef<str>) -> &'a str {
    let lower = lower.as_ref();
    if raw.len() == lower.len() { raw } else { lower }
}

/// Same, for the part of a run after an optional `proto://` head.
fn raw_tail<'a>(raw: &'a str, body: &'a str) -> &'a str {
    if raw.len() >= body.len() && raw.is_char_boundary(raw.len() - body.len()) {
        let t = &raw[raw.len() - body.len()..];
        if t.len() == body.len() { t } else { body }
    } else {
        body
    }
}

fn classify_into(raw: &str, signed: bool, out: &mut Vec<Token>) {
    // Classification reads the lowercased form; what is PUSHED is the
    // original, so the two never disagree about which token this is
    // while still reporting what was written.
    let lower = raw.to_lowercase();
    let _ = &lower;
    // v7.39 (round 651) — the token keeps the text the PARSER saw.
    // Lowercasing is the DICTIONARY's job, which is why `ts_debug`'s
    // `token` column shows `The` while its `lexemes` shows `{}`.
    let push = |out: &mut Vec<Token>, t: &str, ty: TokenType| {
        if !t.is_empty() {
            out.push(Token {
                text: alloc::string::String::from(t),
                ty,
            });
        }
    };
    let ascii = lower.is_ascii();
    let has_alpha = lower.chars().any(char::is_alphabetic);
    let has_digit = lower.chars().any(|c| c.is_ascii_digit());

    // A `proto://` head is its own token and maps to nothing; what
    // follows is judged on its own, exactly as the same string without
    // the head would be.
    let mut body = lower.as_str();
    if let Some(pos) = lower.find("://") {
        push(
            out,
            &alloc::format!("{}://", &lower[..pos]),
            TokenType::Protocol,
        );
        body = &lower[pos + 3..];
    }
    // url vs file, and the discriminator is measured rather than
    // guessed. `ts_debug` on PG18: `http://example.com/a/b` gives url +
    // host + url_path, `http://x.y/z` gives a single `file`, and
    // `http://x.co/z` gives url again — so what decides it is whether
    // the part before the first `/` looks like a HOST, and a one-letter
    // last label does not. An earlier version of this function emitted
    // host and path for every URL because the type list says those
    // types exist; that put `x.y` and `/z` into the index where PG has
    // neither.
    if body.contains('/') {
        let (head, path) = match body.find('/') {
            Some(p) => body.split_at(p),
            None => (body, ""),
        };
        let host_like = head.rsplit_once('.').is_some_and(|(pre, tld)| {
            !pre.is_empty() && tld.len() >= 2 && tld.chars().all(char::is_alphabetic)
        });
        if host_like && !head.is_empty() {
            push(out, raw_tail(raw, body), TokenType::Url);
            push(out, &raw_tail(raw, body)[..head.len()], TokenType::Host);
            push(out, &raw_tail(raw, body)[head.len()..], TokenType::UrlPath);
        } else {
            push(out, raw_tail(raw, body), TokenType::File);
        }
        return;
    }
    let lower = alloc::string::String::from(body);
    let lower = lower.as_str();
    // email: one `@`, something either side, a dot on the right
    if let Some(at) = lower.find('@')
        && at > 0
        && lower[at + 1..].contains('.')
        && !lower[at + 1..].contains('@')
    {
        push(out, raw_of(raw, &lower), TokenType::Email);
        return;
    }
    // hyphenated word: PG emits the compound AND each part
    if lower.contains('-') && has_alpha {
        let compound = if has_digit {
            TokenType::NumHword
        } else if ascii {
            TokenType::AsciiHword
        } else {
            TokenType::Hword
        };
        push(out, raw_of(raw, &lower), compound);
        for (part, raw_part) in lower.split('-').zip(raw_of(raw, &lower).split('-')) {
            if part.is_empty() {
                continue;
            }
            let pty = if part.chars().any(|c| c.is_ascii_digit()) {
                TokenType::HwordNumPart
            } else if part.is_ascii() {
                TokenType::HwordAsciiPart
            } else {
                TokenType::HwordPart
            };
            push(out, raw_part, pty);
        }
        return;
    }
    if lower.contains('.') {
        let dots = lower.matches('.').count();
        let numeric = lower.chars().all(|c| c.is_ascii_digit() || c == '.');
        if numeric && dots >= 2 {
            push(out, raw_of(raw, &lower), TokenType::Version);
            return;
        }
        if numeric && dots == 1 {
            push(out, raw_of(raw, &lower), TokenType::Float);
            return;
        }
        // `1.5e10` — scientific notation
        if dots == 1
            && has_digit
            && lower
                .chars()
                .all(|c| c.is_ascii_digit() || c == '.' || c == 'e' || c == '+' || c == '-')
        {
            push(out, raw_of(raw, &lower), TokenType::SFloat);
            return;
        }
        if has_alpha {
            push(out, raw_of(raw, &lower), TokenType::Host);
            return;
        }
        push(out, raw_of(raw, &lower), TokenType::Version);
        return;
    }
    if !has_alpha && has_digit {
        push(
            out,
            raw_of(raw, &lower),
            if signed {
                TokenType::Int
            } else {
                TokenType::Uint
            },
        );
        return;
    }
    let ty = if has_digit {
        TokenType::NumWord
    } else if ascii {
        TokenType::AsciiWord
    } else {
        TokenType::Word
    };
    push(out, raw_of(raw, &lower), ty);
}

enum WebToken {
    Term(String),
    Phrase(Vec<String>),
    Or,
    /// v7.39 (round 756, F31-B7) — one `-` prefix. PG18-measured: a
    /// dash attaches ACROSS whitespace to the next word or phrase and
    /// STACKS (`- apple` → `!'apple'`, `-"a b"` → `!('a' <-> 'b')`,
    /// `--apple` / `- - apple` → `!!'apple'`); the old tokenizer only
    /// negated a directly-attached word and dropped the rest.
    Neg,
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
        if b == b'-' {
            out.push(WebToken::Neg);
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'"' {
            i += 1;
        }
        let word = &text[start..i];
        if word.eq_ignore_ascii_case("or") {
            out.push(WebToken::Or);
        } else {
            out.push(WebToken::Term(word.to_string()));
        }
    }
    // v7.39 (round 756, F31-B7) — PG18-measured: the word "or" is an
    // OR operator only when it has a left operand and is not the last
    // token. At operand position (start, or right after another OR)
    // and at end of input it is a plain term: 'or apple' → 'or' &
    // 'apple', 'apple or' → 'apple' & 'or', 'or or and -' → 'or' |
    // 'and'. (An operator whose right side comes up empty still
    // vanishes with it — 'apple or -' → 'apple' — which the grouping
    // below already does by dropping empty OR groups.)
    let n = out.len();
    let mut at_operand_pos = true;
    for idx in 0..n {
        if matches!(out[idx], WebToken::Or) && (at_operand_pos || idx + 1 == n) {
            out[idx] = WebToken::Term(String::from("or"));
        }
        // A `-` prefix leaves us still waiting for the operand.
        at_operand_pos = match out[idx] {
            WebToken::Or => true,
            WebToken::Neg => at_operand_pos,
            _ => false,
        };
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
/// v7.39 (round 245) — Snowball's exceptional forms: a handful of words
/// whose stem the algorithm gets wrong are mapped directly (skies→sky and
/// friends), and a few short words are left untouched. PG's english
/// config is the Snowball stemmer, so these are observable in every
/// to_tsvector/to_tsquery differential.
fn stem_exception(word: &str) -> Option<&'static str> {
    Some(match word {
        "skis" => "ski",
        "skies" => "sky",
        "dying" => "die",
        "lying" => "lie",
        "tying" => "tie",
        "idly" => "idl",
        "gently" => "gentl",
        "ugly" => "ugli",
        "early" => "earli",
        "only" => "onli",
        "singly" => "singl",
        "sky" | "news" | "howe" | "atlas" | "cosmos" | "bias" | "andes" => {
            return Some(match word {
                "sky" => "sky",
                "news" => "news",
                "howe" => "howe",
                "atlas" => "atlas",
                "cosmos" => "cosmos",
                "bias" => "bias",
                _ => "andes",
            });
        }
        _ => return None,
    })
}

pub fn porter_stem(word: &str) -> String {
    if let Some(fixed) = stem_exception(word) {
        return String::from(fixed);
    }
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
        // v7.39 (round 245) — Snowball's refinement of Porter's rule:
        // `ies` becomes `ie` when only one letter precedes it (dies→die,
        // ties→tie) and `i` otherwise (cries→cri, flies→fli). The
        // unconditional `i` gave PG-divergent stems for the short words.
        if b.len() - 3 <= 1 {
            replace_suffix(b, 3, b"ie");
        } else {
            replace_suffix(b, 3, b"i");
        }
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
        // v7.39 (round 245) — Snowball's short-word rule (and PG): tie.
        assert_eq!(porter_stem("ties"), "tie");
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
