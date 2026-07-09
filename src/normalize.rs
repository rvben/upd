//! Package-name normalization helpers shared across audit and fix paths.

/// PEP 503 package-name normalization: lowercase and collapse every run of
/// `-`, `_`, `.` into a single `-`. PyPI treats all spelling variants of a
/// project name as the same project, so every PyPI name comparison in upd
/// must go through this canonical form.
pub fn pep503_normalize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_sep = false;
    for c in name.chars() {
        if c == '-' || c == '_' || c == '.' {
            if !prev_sep {
                out.push('-');
                prev_sep = true;
            }
        } else {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            prev_sep = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pep503_lowercases() {
        assert_eq!(pep503_normalize("Django"), "django");
    }

    #[test]
    fn pep503_collapses_underscore_dot_dash_runs() {
        assert_eq!(pep503_normalize("typing_extensions"), "typing-extensions");
        assert_eq!(pep503_normalize("typing.extensions"), "typing-extensions");
        assert_eq!(pep503_normalize("a--b__c..d"), "a-b-c-d");
        assert_eq!(pep503_normalize("mixed-_.runs"), "mixed-runs");
    }

    #[test]
    fn pep503_leaves_canonical_names_unchanged() {
        assert_eq!(pep503_normalize("typing-extensions"), "typing-extensions");
        assert_eq!(pep503_normalize("requests"), "requests");
    }
}
