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

/// Check if a semver version string represents a pre-release.
/// Handles incomplete versions like "0.9" by normalizing to "0.9.0".
/// Returns false for unparseable strings (treated as stable).
pub fn is_prerelease_semver(version_str: &str) -> bool {
    !is_stable_semver(version_str)
}

/// Compare two semver version strings
/// Returns None if either version is invalid
pub fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let va = Version::parse(a).ok()?;
    let vb = Version::parse(b).ok()?;
    Some(va.cmp(&vb))
}

/// The release a crates.io version names, without its semver build metadata.
///
/// Build metadata records how a release was built, so two versions differing
/// only in it are the same release. Cargo ignores it in a requirement and warns
/// on every build that reads one, which is why a Cargo requirement is compared
/// and written without it. Other ecosystems keep theirs: a PEP 440 local
/// version such as `+cpu` selects a different artifact.
pub fn without_build_metadata(version: &str) -> &str {
    version
        .split_once('+')
        .map_or(version, |(release, _)| release)
}

/// The version a Cargo requirement written at `original`'s precision names
/// for `new_version`, without build metadata.
///
/// A stable release keeps as many numeric components as `original` has
/// ("1.0" and "1.2.3" give "1.2"). A pre-release is returned whole: semver
/// has no shorter spelling of one, and cutting `1.0.1-nightly.1` to the dot
/// count of `1.0.0-nightly` names a different pre-release, which an exact
/// requirement would then refuse.
pub fn match_cargo_precision(original: &str, new_version: &str) -> String {
    let new_version = without_build_metadata(new_version);
    if new_version.contains('-') {
        return new_version.to_string();
    }
    let original = without_build_metadata(original);
    let original_release = original.split_once('-').map_or(original, |(r, _)| r);
    let precision = original_release.split('.').count();
    let new_parts: Vec<&str> = new_version.split('.').collect();
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
    fn test_is_prerelease_semver() {
        // Pre-releases must return true
        assert!(is_prerelease_semver("1.0.0-alpha.1"));
        assert!(is_prerelease_semver("1.0.0-beta.2"));
        assert!(is_prerelease_semver("1.0.0-rc.1"));
        assert!(is_prerelease_semver("1.0.0-beta.1"));

        // Stable releases must return false
        assert!(!is_prerelease_semver("1.0.0"));
        assert!(!is_prerelease_semver("2.0.0"));
        assert!(!is_prerelease_semver("0.9"));

        // Unparseable strings must return false (treated as stable)
        assert!(!is_prerelease_semver("*"));
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

    #[test]
    fn build_metadata_is_dropped_and_nothing_else() {
        for (version, release) in [
            ("0.25.13+spec-1.1.0", "0.25.13"),
            ("1.0.0-rc.1+build.5", "1.0.0-rc.1"),
            ("1.0.0-rc.1", "1.0.0-rc.1"),
            ("1.2", "1.2"),
        ] {
            assert_eq!(without_build_metadata(version), release, "{version}");
        }
    }

    #[test]
    fn a_cargo_version_takes_the_current_precision_unless_it_is_a_prerelease() {
        for (original, new, written) in [
            ("1.0", "1.2.3", "1.2"),
            ("1", "2.0.5", "2"),
            ("1.0.0", "1.2.3", "1.2.3"),
            ("1.0.0", "1.2", "1.2"),
            ("0.25.12", "0.25.13+spec-1.1.0", "0.25.13"),
            ("0.25.13+spec-1.1.0", "0.25.15+spec-1.1.0", "0.25.15"),
            ("1.0", "1.2.3+build.4", "1.2"),
            ("1.0.0-nightly", "1.0.1-nightly.1", "1.0.1-nightly.1"),
            (
                "1.0.0-nightly+build.1",
                "1.0.1-nightly.1+build.2",
                "1.0.1-nightly.1",
            ),
            ("1.0.0-beta.1", "1.0.0-beta.2", "1.0.0-beta.2"),
            ("1.0", "1.1.0-rc.1", "1.1.0-rc.1"),
        ] {
            assert_eq!(
                match_cargo_precision(original, new),
                written,
                "{original} -> {new}"
            );
        }
    }
}
