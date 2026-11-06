//! Optional parse-time filters: sets of NPIs, TINs, and billing codes.
//!
//! A filter that is `None` matches everything. Values are compared after
//! trimming and ASCII-uppercasing (codes like `c1785` vs `C1785`).

use std::collections::HashSet;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

/// One set of allowed values, or "allow all".
#[derive(Debug, Clone, Default)]
pub struct ValueFilter(Option<HashSet<String>>);

impl ValueFilter {
    pub fn all() -> ValueFilter {
        ValueFilter(None)
    }

    pub fn from_values<I, S>(values: I) -> ValueFilter
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        ValueFilter(Some(
            values
                .into_iter()
                .map(|v| normalize(v.as_ref()))
                .filter(|v| !v.is_empty())
                .collect(),
        ))
    }

    /// Load from a text file: one value per line; blank lines and lines
    /// starting with `#` are ignored.
    pub fn from_file(path: &Path) -> io::Result<ValueFilter> {
        let f = BufReader::new(std::fs::File::open(path)?);
        let mut values = Vec::new();
        for line in f.lines() {
            let line = line?;
            let t = line.trim();
            if !t.is_empty() && !t.starts_with('#') {
                values.push(t.to_string());
            }
        }
        Ok(ValueFilter::from_values(values))
    }

    pub fn is_all(&self) -> bool {
        self.0.is_none()
    }

    pub fn matches(&self, value: &str) -> bool {
        match &self.0 {
            None => true,
            // Avoid allocating for the common already-normalized case.
            Some(set) => {
                let t = value.trim();
                if t.bytes().any(|b| b.is_ascii_lowercase()) {
                    set.contains(&t.to_ascii_uppercase())
                } else {
                    set.contains(t)
                }
            }
        }
    }

    /// True if any of `values` matches (or the filter allows all).
    pub fn matches_any<'a, I: IntoIterator<Item = &'a str>>(&self, values: I) -> bool {
        self.is_all() || values.into_iter().any(|v| self.matches(v))
    }
}

fn normalize(v: &str) -> String {
    v.trim().to_ascii_uppercase()
}

/// All filters for a run.
#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub npis: ValueFilter,
    pub tins: ValueFilter,
    pub codes: ValueFilter,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_matches_everything() {
        let f = ValueFilter::all();
        assert!(f.matches("anything"));
        assert!(f.matches_any([]));
    }

    #[test]
    fn set_matches_normalized() {
        let f = ValueFilter::from_values(["c1785", " 70551 "]);
        assert!(f.matches("C1785"));
        assert!(f.matches("c1785"));
        assert!(f.matches("70551"));
        assert!(!f.matches("99999"));
        assert!(f.matches_any(["1", "70551"]));
        assert!(!f.matches_any([]));
    }

    #[test]
    fn from_file_skips_comments_and_blanks() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("codes.txt");
        std::fs::write(&p, "# codes\n70551\n\n  C1785 \n").unwrap();
        let f = ValueFilter::from_file(&p).unwrap();
        assert!(f.matches("70551") && f.matches("c1785") && !f.matches("#"));
    }
}
