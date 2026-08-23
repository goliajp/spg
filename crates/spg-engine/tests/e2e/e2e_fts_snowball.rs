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
