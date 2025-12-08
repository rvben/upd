use semver::Version;

/// Check if a semver version string represents a stable release
pub fn is_stable_semver(version_str: &str) -> bool {
    if let Ok(version) = Version::parse(version_str) {
        version.pre.is_empty()
    } else {
        false
    }
}

/// Compare two semver version strings
/// Returns None if either version is invalid
pub fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let va = Version::parse(a).ok()?;
    let vb = Version::parse(b).ok()?;
    Some(va.cmp(&vb))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stable_versions() {
        assert!(is_stable_semver("1.0.0"));
        assert!(is_stable_semver("2.31.0"));
        assert!(is_stable_semver("0.1.0"));
    }

    #[test]
    fn test_prerelease_versions() {
        assert!(!is_stable_semver("1.0.0-alpha.1"));
        assert!(!is_stable_semver("1.0.0-beta.2"));
        assert!(!is_stable_semver("1.0.0-rc.1"));
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
