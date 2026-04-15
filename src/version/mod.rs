pub mod pep440;
pub mod semver_util;

pub use pep440::is_stable_pep440;
pub use semver_util::is_stable_semver;

/// Match the precision of a new version to the original version's precision.
///
/// For PEP 440 versions (Python), the release segment length is determined by
/// parsing with `pep440_rs`, so post-release (`.post1`), dev (`.dev0`), and
/// pre-release (`a1`, `b1`, `rc1`) suffixes are not counted as release segments
/// and are preserved when the release segment count matches.
///
/// For other version schemes (semver, Go, Ruby…) that `pep440_rs` cannot parse,
/// the function falls back to counting dot-separated segments.
///
/// Examples:
/// - ("2.0", "3.0.5") → "3.0"
/// - ("2.0.0", "3.0.5") → "3.0.5"
/// - ("2", "3.0.5") → "3"
/// - ("1.2.3.4", "5.6.7.8") → "5.6.7.8" (preserve all parts)
/// - ("2.1117.0", "2.1117.0.post1") → "2.1117.0.post1" (post-release preserved)
pub fn match_version_precision(original: &str, new_version: &str) -> String {
    use pep440_rs::Version;

    // Try PEP 440 parsing first; `.release()` returns only the numeric
    // release tuple, correctly excluding pre/post/dev labels.
    if let (Ok(orig), Ok(new)) = (original.parse::<Version>(), new_version.parse::<Version>()) {
        let orig_len = orig.release().len();
        let new_len = new.release().len();

        return if orig_len >= new_len {
            // Same or fewer release segments — keep the full new version string
            // (including any post/pre/dev suffix) as written by the caller.
            new_version.to_string()
        } else {
            // More release segments than original — truncate to original precision.
            // Any post/pre/dev suffix on the new version is intentionally dropped
            // since it belongs to a more specific release than we're tracking.
            new.release()[..orig_len]
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(".")
        };
    }

    // Fallback for non-PEP-440 versions (semver, Go modules, etc.)
    let original_parts: Vec<&str> = original.split('.').collect();
    let new_parts: Vec<&str> = new_version.split('.').collect();
    let precision = original_parts.len();

    if precision >= new_parts.len() {
        new_version.to_string()
    } else {
        new_parts[..precision].join(".")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_match_version_precision() {
        // Major.minor only
        assert_eq!(match_version_precision("2.0", "3.0.5"), "3.0");
        assert_eq!(match_version_precision("2.8", "3.1.2"), "3.1");

        // Full precision
        assert_eq!(match_version_precision("2.0.0", "3.0.5"), "3.0.5");
        assert_eq!(match_version_precision("1.2.3", "4.5.6"), "4.5.6");

        // Major only
        assert_eq!(match_version_precision("2", "3.0.5"), "3");

        // New version has fewer parts than original (edge case)
        assert_eq!(match_version_precision("2.0.0", "3.0"), "3.0");
        assert_eq!(match_version_precision("2.0.0.0", "3.0.5"), "3.0.5");

        // Same precision
        assert_eq!(match_version_precision("1.0.0", "2.0.0"), "2.0.0");

        // PEP 440 post-release suffix must be preserved when release precision matches
        assert_eq!(
            match_version_precision("2.1117.0", "2.1117.0.post1"),
            "2.1117.0.post1"
        );
        assert_eq!(
            match_version_precision("1.0.0", "1.0.0.post2"),
            "1.0.0.post2"
        );

        // PEP 440 pre-release labels (no dot separator) are also preserved
        assert_eq!(match_version_precision("2.0.0", "2.0.1a1"), "2.0.1a1");

        // Truncation path: suffix is dropped when original has lower precision
        assert_eq!(match_version_precision("2.0", "3.0.5.post1"), "3.0");
    }
}
