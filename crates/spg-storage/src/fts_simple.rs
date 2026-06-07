//! v7.17.0 Phase 2.2 — lower-cased word tokenisation that mirrors
//! `to_tsvector('simple', text)`. Lives in storage (not engine)
//! because the [`crate::IndexKind::GinFulltext`] posting-list
//! build / rebuild / insert paths all run in storage and the
//! engine crate can't be a build-time dependency from here.
//!
//! The rule is intentionally tiny — same one that PG's `simple`
//! text-search config uses and that `spg-engine::fts::tokenize`
//! exposes for `to_tsvector` evaluation:
//!
//!   * keep alphanumeric Unicode scalars + ASCII `_`
//!   * lower-case as we collect
//!   * everything else splits a token
//!   * empty tokens are dropped
//!
//! No stopword drop, no Porter stem — the MySQL FULLTEXT mapping
//! deliberately uses the cheapest config so MATCH AGAINST term
//! matching stays close to MyISAM's behaviour (substring + word
//! boundary, no language-aware suffix folding).
//!
//! Order of returned lexemes is **input order with duplicates
//! preserved**. Callers that want the canonical sorted-deduped
//! `tsvector` shape (e.g. `Value::TsVector`) must do their own
//! merge — the storage-side posting-list builder is fine with
//! the as-is stream because each `map.insert_mut(word, ..)`
//! append is idempotent on the posting list.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// Tokenise `text` into lower-cased word tokens, splitting on
/// every non-alphanumeric / non-`_` scalar. See module-level
/// doc for the full rule.
pub fn simple_lex(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercase_ascii_words() {
        assert_eq!(
            simple_lex("Hello, World! Quick BROWN fox."),
            alloc::vec![
                String::from("hello"),
                String::from("world"),
                String::from("quick"),
                String::from("brown"),
                String::from("fox")
            ]
        );
    }

    #[test]
    fn keeps_underscore_inside_word() {
        assert_eq!(
            simple_lex("post_title COMMENT_BODY"),
            alloc::vec![String::from("post_title"), String::from("comment_body")]
        );
    }

    #[test]
    fn collapses_runs_of_separators() {
        assert_eq!(
            simple_lex("  --- foo   ,, bar . . . baz "),
            alloc::vec![
                String::from("foo"),
                String::from("bar"),
                String::from("baz")
            ]
        );
    }

    #[test]
    fn empty_input_yields_empty() {
        assert!(simple_lex("").is_empty());
        assert!(simple_lex(",,, ...").is_empty());
    }
}
