//! Compiled matching for `--package` selectors.
//!
//! Package selectors are globs rather than filesystem paths: matching is
//! case-sensitive, and `*`/`?` may cross `/` so one pattern can select scoped
//! npm packages, Go modules, GitHub Actions, and similar names. Exact names are
//! ordinary globs without metacharacters and retain their previous behaviour.

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Debug)]
struct PackageFilterInner {
    patterns: Vec<String>,
    matcher: GlobSet,
    matched: Vec<AtomicBool>,
}

/// A compiled, shareable set of package-name selectors.
///
/// Clones share both the compiled matcher and match bookkeeping. This lets the
/// parallel updater jobs report a glob that matched nothing once per run,
/// without recompiling patterns for every dependency.
#[derive(Clone, Debug)]
pub struct PackageFilter {
    inner: Arc<PackageFilterInner>,
}

impl Default for PackageFilter {
    fn default() -> Self {
        Self::new(Vec::new()).expect("an empty package filter is valid")
    }
}

impl PackageFilter {
    /// Compile package-name patterns.
    ///
    /// Supported metacharacters are `*`, `?`, and character classes such as
    /// `[abc]` and `[!abc]`. Brace alternation is deliberately excluded: the
    /// CLI already uses commas to separate selectors, and repeatable selectors
    /// make the same choice without an ambiguous second comma grammar.
    pub fn new(patterns: Vec<String>) -> Result<Self, String> {
        let mut builder = GlobSetBuilder::new();
        for pattern in &patterns {
            builder.add(compile_pattern(pattern)?);
        }
        let matcher = builder
            .build()
            .map_err(|error| format!("invalid package pattern: {error}"))?;
        let matched = (0..patterns.len())
            .map(|_| AtomicBool::new(false))
            .collect();
        Ok(Self {
            inner: Arc::new(PackageFilterInner {
                patterns,
                matcher,
                matched,
            }),
        })
    }

    /// The selectors exactly as supplied on the command line.
    pub fn patterns(&self) -> &[String] {
        &self.inner.patterns
    }

    /// Whether any package restriction is active.
    pub fn is_empty(&self) -> bool {
        self.inner.patterns.is_empty()
    }

    /// Return whether `package` is selected and record every matching pattern.
    /// An empty filter selects every package.
    pub fn matches(&self, package: &str) -> bool {
        if self.is_empty() {
            return true;
        }

        let indices = self.inner.matcher.matches(package);
        if indices.is_empty() {
            return false;
        }

        for index in indices {
            self.inner.matched[index].store(true, Ordering::Release);
        }
        true
    }

    /// Glob selectors that have not matched a package in this run.
    ///
    /// Exact names remain silent when absent, preserving the historical
    /// `--package missing` no-op. A glob is more likely to contain a typo, so
    /// callers surface these as warnings while retaining exit code 0.
    pub fn unmatched_globs(&self) -> Vec<String> {
        self.inner
            .patterns
            .iter()
            .zip(self.inner.matched.iter())
            .filter(|(pattern, was_matched)| {
                is_glob(pattern) && !was_matched.load(Ordering::Acquire)
            })
            .map(|(pattern, _)| pattern.clone())
            .collect()
    }
}

/// Validate one value before clap accepts it. Returning the original string
/// lets clap continue to own comma splitting and repeated-flag collection.
pub(crate) fn parse_package_pattern(pattern: &str) -> Result<String, String> {
    compile_pattern(pattern)?;
    Ok(pattern.to_string())
}

fn compile_pattern(pattern: &str) -> Result<globset::Glob, String> {
    if pattern.is_empty() {
        return Err("package pattern cannot be empty".to_string());
    }
    if contains_unescaped_brace(pattern) {
        return Err(format!(
            "invalid package pattern '{pattern}': brace alternation is not supported; use repeated or comma-separated --package values"
        ));
    }

    GlobBuilder::new(pattern)
        .case_insensitive(false)
        .literal_separator(false)
        .backslash_escape(true)
        .build()
        .map_err(|error| format!("invalid package pattern '{pattern}': {error}"))
}

fn contains_unescaped_brace(pattern: &str) -> bool {
    let mut escaped = false;
    for ch in pattern.chars() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if matches!(ch, '{' | '}') {
            return true;
        }
    }
    false
}

fn is_glob(pattern: &str) -> bool {
    let mut escaped = false;
    for ch in pattern.chars() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if matches!(ch, '*' | '?' | '[') {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_names_remain_exact_and_case_sensitive() {
        let filter = PackageFilter::new(vec!["shiny".to_string()]).unwrap();
        assert!(filter.matches("shiny"));
        assert!(!filter.matches("Shiny"));
        assert!(!filter.matches("shinywidgets"));
    }

    #[test]
    fn wildcard_forms_match_package_names() {
        let filter = PackageFilter::new(vec![
            "shiny*".to_string(),
            "crate-?".to_string(),
            "lib[ab]".to_string(),
        ])
        .unwrap();
        assert!(filter.matches("shinywidgets"));
        assert!(filter.matches("crate-a"));
        assert!(filter.matches("libb"));
        assert!(!filter.matches("libc"));
    }

    #[test]
    fn star_crosses_package_namespace_separators() {
        let filter = PackageFilter::new(vec!["@scope/*".to_string()]).unwrap();
        assert!(filter.matches("@scope/pkg"));
        assert!(filter.matches("@scope/nested/pkg"));
    }

    #[test]
    fn overlapping_patterns_are_each_recorded_as_matched() {
        let filter =
            PackageFilter::new(vec!["shiny*".to_string(), "*widgets".to_string()]).unwrap();
        assert!(filter.matches("shinywidgets"));
        assert!(filter.unmatched_globs().is_empty());
    }

    #[test]
    fn only_unmatched_globs_are_reported() {
        let filter = PackageFilter::new(vec!["exact".to_string(), "missing*".to_string()]).unwrap();
        assert_eq!(filter.unmatched_globs(), vec!["missing*".to_string()]);
    }

    #[test]
    fn invalid_or_ambiguous_patterns_are_rejected() {
        assert!(parse_package_pattern("broken[").is_err());
        assert!(parse_package_pattern("{foo,bar}").is_err());
        assert!(parse_package_pattern("").is_err());
    }
}
