//! Output normalization — MTR's `replace_result`/`replace_regex` idea
//! with the rules as DATA (S2.5, design D5-adjacent). Zero-dep, so the
//! "regex" layer is three hand-rolled scanners for the shapes test
//! output actually varies by, plus literal pairs from the rules file.
//!
//! Consumers: the three-oracle differential (S3.6) normalizes both
//! legs before comparing; `--record --oracle` corpora that carry
//! environment-dependent cells.

/// One normalization pass: builtin classes first, then literal pairs.
#[derive(Debug, Default)]
pub struct Rules {
    pub timestamps: bool,
    pub paths: bool,
    pub versions: bool,
    pub literals: Vec<(String, String)>,
}

impl Rules {
    /// Parse the rules file (same loud TOML subset as the manifest).
    ///
    /// ```toml
    /// timestamps = 1
    /// paths = 1
    /// versions = 1
    /// [[replace]]
    /// from = "mini.local"
    /// to = "<HOST>"
    /// ```
    ///
    /// # Errors
    /// Unknown keys — a shrugged-off rule is a diff that flaps later.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut r = Rules::default();
        let mut in_replace = false;
        let (mut from, mut to): (Option<String>, Option<String>) = (None, None);
        let flush = |from: &mut Option<String>,
                     to: &mut Option<String>,
                     lits: &mut Vec<(String, String)>| {
            if let (Some(f), Some(t)) = (from.take(), to.take()) {
                lits.push((f, t));
            }
        };
        for (ln, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line == "[[replace]]" {
                flush(&mut from, &mut to, &mut r.literals);
                in_replace = true;
                continue;
            }
            let (k, v) = line
                .split_once('=')
                .ok_or_else(|| format!("norm-rules:{}: expected key = value", ln + 1))?;
            let (k, v) = (k.trim(), v.trim());
            let unq = v.trim_matches('"').to_string();
            match (in_replace, k) {
                (false, "timestamps") => r.timestamps = v == "1",
                (false, "paths") => r.paths = v == "1",
                (false, "versions") => r.versions = v == "1",
                (true, "from") => from = Some(unq),
                (true, "to") => to = Some(unq),
                _ => return Err(format!("norm-rules:{}: unknown key {k}", ln + 1)),
            }
        }
        flush(&mut from, &mut to, &mut r.literals);
        Ok(r)
    }

    /// Apply every enabled rule to one line.
    #[must_use]
    pub fn apply(&self, line: &str) -> String {
        let mut s = line.to_string();
        if self.timestamps {
            s = scrub_timestamps(&s);
        }
        if self.versions {
            s = scrub_versions(&s);
        }
        if self.paths {
            s = scrub_paths(&s);
        }
        for (f, t) in &self.literals {
            s = s.replace(f.as_str(), t);
        }
        s
    }
}

/// `YYYY-MM-DD HH:MM:SS[.frac][+TZ]` and the `T` spelling → `<TIMESTAMP>`.
fn scrub_timestamps(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        // Candidate: dddd-dd-dd
        if i + 10 <= b.len()
            && b[i..i + 4].iter().all(u8::is_ascii_digit)
            && b[i + 4] == b'-'
            && b[i + 5..i + 7].iter().all(u8::is_ascii_digit)
            && b[i + 7] == b'-'
            && b[i + 8..i + 10].iter().all(u8::is_ascii_digit)
        {
            let mut j = i + 10;
            // Optional time part.
            if j + 9 <= b.len()
                && (b[j] == b' ' || b[j] == b'T')
                && b[j + 1..j + 3].iter().all(u8::is_ascii_digit)
                && b[j + 3] == b':'
            {
                j += 9; // " HH:MM:SS"
                while j < b.len()
                    && (b[j].is_ascii_digit() || matches!(b[j], b'.' | b'+' | b'-' | b':'))
                {
                    j += 1;
                }
                out.push_str("<TIMESTAMP>");
                i = j;
                continue;
            }
            out.push_str("<DATE>");
            i += 10;
            continue;
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

/// `vX.Y.Z` / `X.Y.Z` version triples → `<VERSION>`.
fn scrub_versions(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let start = if b[i] == b'v' && i + 1 < b.len() && b[i + 1].is_ascii_digit() {
            i + 1
        } else if b[i].is_ascii_digit() && (i == 0 || !b[i - 1].is_ascii_alphanumeric()) {
            i
        } else {
            out.push(b[i] as char);
            i += 1;
            continue;
        };
        // digits '.' digits '.' digits
        let mut j = start;
        let mut dots = 0;
        while j < b.len() && (b[j].is_ascii_digit() || (b[j] == b'.' && dots < 2)) {
            if b[j] == b'.' {
                dots += 1;
            }
            j += 1;
        }
        if dots == 2 && b[j - 1].is_ascii_digit() {
            out.push_str("<VERSION>");
            i = j;
        } else {
            out.push(b[i] as char);
            i += 1;
        }
    }
    out
}

/// Absolute unix paths → `<PATH>` (stops at whitespace or quote).
fn scrub_paths(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'/'
            && i + 1 < b.len()
            && (b[i + 1].is_ascii_alphanumeric() || b[i + 1] == b'_')
            && (i == 0 || !b[i - 1].is_ascii_alphanumeric())
        {
            let mut j = i;
            while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'"' && b[j] != b'\'' {
                j += 1;
            }
            // Only treat as a path when it has at least two segments.
            if s[i..j].matches('/').count() >= 2 {
                out.push_str("<PATH>");
                i = j;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_dates_versions_paths_each_scrub() {
        let r = Rules {
            timestamps: true,
            paths: true,
            versions: true,
            literals: vec![("mini.local".into(), "<HOST>".into())],
        };
        assert_eq!(
            r.apply("at 2026-08-17 10:31:02.123+00 ok"),
            "at <TIMESTAMP> ok"
        );
        assert_eq!(r.apply("born 2026-08-17."), "born <DATE>.");
        assert_eq!(
            r.apply("spg v7.37.29 and 1.2.3"),
            "spg <VERSION> and <VERSION>"
        );
        assert_eq!(
            r.apply("log at /tmp/spg-suite/x.log on mini.local"),
            "log at <PATH> on <HOST>"
        );
        // Non-matches survive untouched.
        assert_eq!(r.apply("1.5 rows in 3/4 time"), "1.5 rows in 3/4 time");
    }

    #[test]
    fn rules_file_round_trips_and_rejects_unknown_keys() {
        let r = Rules::parse("timestamps = 1\npaths = 0\n[[replace]]\nfrom = \"a\"\nto = \"b\"\n")
            .unwrap();
        assert!(r.timestamps && !r.paths);
        assert_eq!(r.literals, vec![("a".into(), "b".into())]);
        assert!(Rules::parse("mystery = 1\n").is_err());
    }
}
