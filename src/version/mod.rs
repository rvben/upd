pub mod compare;
pub mod gradle;
pub mod pep440;
pub mod semver_util;
pub mod tag;

pub use compare::compare_versions;
pub use pep440::{is_prerelease_pep440, is_stable_pep440};
pub use semver_util::{
    is_prerelease_semver, is_stable_semver, match_cargo_precision, without_build_metadata,
};
pub use tag::TagVersion;

use crate::updater::FileType;

/// Match `latest` to the precision and `v` prefix that `current` declares.
///
/// A git ref names a tag, so the `v` belongs to the declaration rather than to
/// the release: shortening `v4.1.2` to `4` names a ref the repository does not
/// publish. Both parts of the declaration's shape therefore survive the rewrite.
pub fn match_precision_with_prefix(current: &str, latest: &str, full_precision: bool) -> String {
    let bare_current = current.strip_prefix('v').unwrap_or(current);
    let bare_latest = latest.strip_prefix('v').unwrap_or(latest);
    let matched = if full_precision {
        bare_latest.to_string()
    } else {
        match_version_precision(bare_current, bare_latest)
    };
    crate::annotation::reapply_v_prefix(current, &matched)
}

/// The string a writer puts in a file of `file_type` when raising `original` to
/// `new_version`.
///
/// Writing a version is the only way to raise one, so this answers what any
/// caller proposing an edit would actually produce. Asking it before scheduling
/// an edit is what keeps a report of pending work and the work itself the same
/// question: a target equal to what is already declared is no edit at all.
pub fn written_version(
    file_type: FileType,
    original: &str,
    new_version: &str,
    full_precision: bool,
) -> String {
    match (file_type, full_precision) {
        // One rule for both Cargo writers; see `match_cargo_precision`.
        (FileType::CargoToml, true) => without_build_metadata(new_version).to_string(),
        (FileType::CargoToml, false) => match_cargo_precision(original, new_version),
        // Gradle versions are written whole, by a writer that takes the target
        // before any precision matching.
        (FileType::GradleCatalog | FileType::GradleScript | FileType::GradleWrapper, _) => {
            new_version.to_string()
        }
        // Refs carry their own prefix.
        (FileType::GithubActions | FileType::PreCommitConfig, _) => {
            match_precision_with_prefix(original, new_version, full_precision)
        }
        (FileType::Annotated, true) => crate::annotation::reapply_v_prefix(original, new_version),
        (FileType::Annotated, false) => crate::annotation::reapply_v_prefix(
            original,
            &match_version_precision(original, new_version),
        ),
        (_, true) => new_version.to_string(),
        (_, false) => match_version_precision(original, new_version),
    }
}

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
            // Same or fewer release segments - keep the full new version string
            // (including any post/pre/dev suffix) as written by the caller.
            new_version.to_string()
        } else {
            // More release segments than original - truncate to original precision.
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
        assert_eq!(match_version_precision("2.0.0", "2.0.1b1"), "2.0.1b1");
        assert_eq!(match_version_precision("2.0.0", "2.0.1rc1"), "2.0.1rc1");

        // PEP 440 dev releases are also preserved
        assert_eq!(match_version_precision("2.0.0", "2.0.0.dev1"), "2.0.0.dev1");

        // Truncation path: suffix is dropped when original has lower precision
        assert_eq!(match_version_precision("2.0", "3.0.5.post1"), "3.0");
    }
}
