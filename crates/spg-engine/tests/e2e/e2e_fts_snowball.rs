//! v7.38.18 — the non-English stemmers against PostgreSQL 18.4, word
//! for word.
//!
//! Each stemmer is implemented from Snowball's published algorithm, and
//! what decides whether it is right is the oracle: `xtests/fts_gold/`
//! holds `word|lexeme` for a vocabulary built to reach every suffix
//! class the algorithm names, read straight off `to_tsvector` on PG
//! 18.4. A stemmer is the kind of code that is 99% right and silently
//! wrong on the rest, which is why the corpus is thousands of words
//! rather than the dozen a hand-written test would carry.
//!
//! The Spanish corpus was built in two halves for the same reason: the
//! first while writing the stemmer, the second held back and run only
//! afterwards. The held-back half found seven wrong words the first had
//! not -- `competencia`, `responsabilidad` and five relatives, all of
//! them suffix groups that REPLACE rather than delete.

use std::path::PathBuf;

fn gold(name: &str) -> Vec<(String, String)> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../xtests/fts_gold")
        .join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text.lines()
        .filter_map(|l| l.split_once('|'))
        // A stopword has no lexeme, and a word that tokenises into more
        // than one is not a stemming question.
        .filter(|(_, v)| *v != "<STOP>" && !v.contains(','))
        .map(|(w, v)| (w.to_string(), v.to_string()))
        .collect()
}

/// How many words each corpus actually puts to the engine, after the
/// stopwords and the multi-lexeme entries `gold` drops.
///
/// v7.38.18 — these are pinned because a customer letter quoted them
/// and got them wrong. It said "6,120 words -- 1,874 Spanish, 2,229
/// French, 2,017 German", and two of those three were FILE LINE COUNTS
/// while the third was a compared-word count. The total matched neither
/// reading: the files hold 6,183 lines and the comparison sees 6,057.
///
/// A number that appears in something we send someone has to be
/// asserted somewhere, or it is a number that was true once.
const GOLD_COUNTS: [(&str, usize); 3] =
    [
    ("spanish.tsv", 1847),
    ("french.tsv", 2193),
    ("german.tsv", 2017),
];

#[test]
fn the_gold_corpora_are_the_size_the_documents_claim() {
    let total: usize = GOLD_COUNTS
        .iter()
        .map(|(f, want)| {
            let got = gold(f).len();
            assert_eq!(
                got, *want,
                "{f}: the corpus compares {got} words, the documents say {want} \
                 -- update CHANGELOG.md, PG_MIGRATION.md and the customer letter \
                 together with this constant"
            );
            got
        })
        .sum();
    assert_eq!(total, 6057, "the total quoted in the documents");
}

fn check(config: &str, file: &str) {
    let mut e = spg_engine::Engine::new();
    let rows = gold(file);
    assert!(rows.len() > 400, "{file}: only {} words", rows.len());
    let mut wrong: Vec<String> = Vec::new();
    for (word, want) in &rows {
        let sql = format!(
            "SELECT to_tsvector('{config}', '{}')::text",
            word.replace('\'', "''")
        );
        let got = match e
            .execute(&sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
        {
            spg_engine::QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
                spg_storage::Value::Text(t) => t.to_string(),
                other => format!("{other:?}"),
            },
            other => panic!("{other:?}"),
        };
        // `to_tsvector(w)::text` is `'lexeme':1`; the position is not
        // what this is about.
        let lex = got
            .rsplit_once(':')
            .map_or(got.as_str(), |(l, _)| l)
            .trim_matches('\'')
            .to_string();
        if lex != *want && wrong.len() < 20 {
            wrong.push(format!("{word} → {lex} (PG {want})"));
        }
    }
    assert!(
        wrong.is_empty(),
        "{config}: {} of {} words disagree with PG 18.4:\n{}",
        wrong.len(),
        rows.len(),
        wrong.join("\n")
    );
}

#[test]
fn spanish_stems_as_pg_does() {
    check("spanish", "spanish.tsv");
}

#[test]
fn french_stems_as_pg_does() {
    check("french", "french.tsv");
}

#[test]
fn german_stems_as_pg_does() {
    check("german", "german.tsv");
}
