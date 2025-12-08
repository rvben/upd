use semver::Version;

/// Normalize a version string to full semver format (MAJOR.MINOR.PATCH)
/// "1" -> "1.0.0", "1.2" -> "1.2.0", "1.2.3" -> "1.2.3"
fn normalize_version(version_str: &str) -> String {
    // Handle prerelease suffix (e.g., "1.0-alpha" -> keep as is after normalization)
    let (base, suffix) = if let Some(idx) = version_str.find('-') {
        (&version_str[..idx], &version_str[idx..])
    } else {
        (version_str, "")
    };

    let parts: Vec<&str> = base.split('.').collect();
    let normalized = match parts.len() {
        1 => format!("{}.0.0", parts[0]),
        2 => format!("{}.{}.0", parts[0], parts[1]),
        _ => base.to_string(),
    };

    format!("{}{}", normalized, suffix)
}

/// Check if a semver version string represents a stable release
/// Handles incomplete versions like "0.9" by normalizing to "0.9.0"
pub fn is_stable_semver(version_str: &str) -> bool {
    let normalized = normalize_version(version_str);
    if let Ok(version) = Version::parse(&normalized) {
        version.pre.is_empty()
    } else {
        // If it still can't parse, assume it's stable (e.g., "*" or complex constraints)
        // This is safer than treating unknown formats as prereleases
        true
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
    fn test_normalize_version() {
        assert_eq!(normalize_version("1"), "1.0.0");
        assert_eq!(normalize_version("1.2"), "1.2.0");
        assert_eq!(normalize_version("1.2.3"), "1.2.3");
        assert_eq!(normalize_version("0.9"), "0.9.0");
        assert_eq!(normalize_version("1.0-alpha"), "1.0.0-alpha");
        assert_eq!(normalize_version("1-beta.1"), "1.0.0-beta.1");
    }

    #[test]
    fn test_stable_versions() {
        assert!(is_stable_semver("1.0.0"));
        assert!(is_stable_semver("2.31.0"));
        assert!(is_stable_semver("0.1.0"));
    }

    #[test]
    fn test_incomplete_versions_are_stable() {
        // Incomplete versions like "0.9" should be treated as stable
        assert!(is_stable_semver("0.9"));
        assert!(is_stable_semver("1"));
        assert!(is_stable_semver("2.0"));
    }

    #[test]
    fn test_prerelease_versions() {
        assert!(!is_stable_semver("1.0.0-alpha.1"));
        assert!(!is_stable_semver("1.0.0-beta.2"));
        assert!(!is_stable_semver("1.0.0-rc.1"));
        // Incomplete versions with prerelease suffix
        assert!(!is_stable_semver("1.0-alpha"));
        assert!(!is_stable_semver("0.9-rc1"));
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
