use pep440_rs::Version;

/// Check if a PEP 440 version string represents a stable release
pub fn is_stable_pep440(version_str: &str) -> bool {
    if let Ok(version) = version_str.parse::<Version>() {
        !version.is_pre() && !version.is_dev()
    } else {
        false
    }
}

/// Check if a PEP 440 version string represents a pre-release
/// (alpha, beta, release candidate, or dev release).
/// Returns false for unparseable strings (treated as stable).
pub fn is_prerelease_pep440(version_str: &str) -> bool {
    if let Ok(version) = version_str.parse::<Version>() {
        version.is_pre() || version.is_dev()
    } else {
        false
    }
}

/// Compare two PEP 440 version strings
/// Returns None if either version is invalid
pub fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let va = a.parse::<Version>().ok()?;
    let vb = b.parse::<Version>().ok()?;
    Some(va.cmp(&vb))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stable_versions() {
        assert!(is_stable_pep440("1.0.0"));
        assert!(is_stable_pep440("2.31.0"));
        assert!(is_stable_pep440("0.1.0"));
        assert!(is_stable_pep440("1.0"));
        assert!(is_stable_pep440("1"));
        // Post-releases are stable releases that fix packaging issues
        assert!(is_stable_pep440("1.0.0.post1"));
        assert!(is_stable_pep440("2.1117.0.post1"));
    }

    #[test]
    fn test_prerelease_versions() {
        assert!(!is_stable_pep440("1.0.0a1"));
        assert!(!is_stable_pep440("1.0.0b2"));
        assert!(!is_stable_pep440("1.0.0rc1"));
        assert!(!is_stable_pep440("1.0.0.dev1"));
        assert!(!is_stable_pep440("1.0.0alpha"));
        assert!(!is_stable_pep440("1.0.0beta"));
    }

    #[test]
    fn test_is_prerelease_pep440() {
        // Pre-releases must return true
        assert!(is_prerelease_pep440("1.0.0a1"));
        assert!(is_prerelease_pep440("1.0.0b2"));
        assert!(is_prerelease_pep440("1.0.0rc1"));
        assert!(is_prerelease_pep440("1.0.0.dev1"));
        assert!(is_prerelease_pep440("25.0.0b1"));

        // Stable releases must return false
        assert!(!is_prerelease_pep440("1.0.0"));
        assert!(!is_prerelease_pep440("2.31.0"));
        assert!(!is_prerelease_pep440("1.0.0.post1"));

        // Unparseable strings must return false (treated as stable)
        assert!(!is_prerelease_pep440("not-a-version"));
        assert!(!is_prerelease_pep440(""));
    }

    #[test]
    fn test_version_comparison() {
        assert_eq!(
            compare_versions("1.0.0", "2.0.0"),
            Some(std::cmp::Ordering::Less)
        );
        assert_eq!(
            compare_versions("2.0.0", "1.0.0"),
            Some(std::cmp::Ordering::Greater)
        );
        assert_eq!(
            compare_versions("1.0.0", "1.0.0"),
            Some(std::cmp::Ordering::Equal)
        );
    }
}
