pub mod pep440;
pub mod semver_util;

pub use pep440::is_stable_pep440;
pub use semver_util::is_stable_semver;

/// Match the precision of a new version to the original version's precision.
///
/// Examples:
/// - ("2.0", "3.0.5") → "3.0"
/// - ("2.0.0", "3.0.5") → "3.0.5"
/// - ("2", "3.0.5") → "3"
/// - ("1.2.3.4", "5.6.7.8") → "5.6.7.8" (preserve all parts)
pub fn match_version_precision(original: &str, new_version: &str) -> String {
    let original_parts: Vec<&str> = original.split('.').collect();
    let new_parts: Vec<&str> = new_version.split('.').collect();

    // Count how many parts the original has
    let precision = original_parts.len();

    // Take the same number of parts from the new version
    if precision >= new_parts.len() {
        // Original has same or more precision, use full new version
        new_version.to_string()
    } else {
        // Original has less precision, truncate new version
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
    }
}
