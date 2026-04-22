//! `TagVersion`: a version type for GitHub-style git tags with arbitrary
//! numeric release-segment counts.
//!
//! Exists because `semver::Version` requires exactly `MAJOR.MINOR.PATCH` and
//! rejects 4+ segment tags (e.g. `shellcheck-py` uses `v0.11.0.1`), which
//! caused `upd` to silently pick wrong "latest" tags.

use std::cmp::Ordering;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TagVersion {
    release: Vec<u64>,
    prerelease: Option<String>,
}

impl TagVersion {
    /// Parse a tag string of the form `v?<n>(.<n>)*[-<pre>][+<build>]`.
    /// Returns `None` if the release part is missing, empty, or non-numeric.
    /// Build metadata is dropped (per semver, it does not affect identity or ordering).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.strip_prefix('v').unwrap_or(s);
        if s.is_empty() {
            return None;
        }

        // Strip build metadata first: everything after the first '+'.
        let s = s.split('+').next().unwrap_or(s);

        // Split off prerelease suffix at the first '-'.
        let (release_str, prerelease) = match s.split_once('-') {
            Some((r, p)) if !p.is_empty() => (r, Some(p.to_string())),
            _ => (s, None),
        };

        if release_str.is_empty() {
            return None;
        }

        let mut release = Vec::new();
        for part in release_str.split('.') {
            // Reject empty segments ("1..3", "1.2.") and non-numeric chars.
            let n: u64 = part.parse().ok()?;
            release.push(n);
        }

        Some(Self {
            release,
            prerelease,
        })
    }

    /// Returns `true` if this version has a hyphen-delimited prerelease suffix.
    ///
    /// Any hyphen suffix (e.g. `-rc.1`, `-beta`, `-1`) is treated as a
    /// prerelease marker. Tags like `v0.7.0.1-1` are therefore classified as
    /// prereleases even though some ecosystems use them as packaging patches.
    pub fn is_prerelease(&self) -> bool {
        self.prerelease.is_some()
    }
}

impl Ord for TagVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        // Compare release segments with implicit trailing zeros.
        let max_len = self.release.len().max(other.release.len());
        for i in 0..max_len {
            let a = self.release.get(i).copied().unwrap_or(0);
            let b = other.release.get(i).copied().unwrap_or(0);
            match a.cmp(&b) {
                Ordering::Equal => continue,
                other => return other,
            }
        }

        // Release segments are equal. Semver rule: a prerelease is less than
        // the same release without a prerelease suffix.
        match (&self.prerelease, &other.prerelease) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            // Prerelease strings are compared lexically. This gives a total
            // order but does not match semantic intent for numeric suffixes
            // like "rc.10" vs "rc.2". Good enough for picking a maximum tag
            // out of a release stream.
            (Some(a), Some(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for TagVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    // ---- parse ----

    #[test]
    fn parses_three_segment_with_v_prefix() {
        let v = TagVersion::parse("v1.2.3").unwrap();
        assert_eq!(v.release, vec![1, 2, 3]);
        assert_eq!(v.prerelease, None);
    }

    #[test]
    fn parses_four_segment_shellcheck_py_style() {
        let v = TagVersion::parse("v0.11.0.1").unwrap();
        assert_eq!(v.release, vec![0, 11, 0, 1]);
        assert_eq!(v.prerelease, None);
    }

    #[test]
    fn parses_without_v_prefix() {
        let v = TagVersion::parse("24.3.0").unwrap();
        assert_eq!(v.release, vec![24, 3, 0]);
    }

    #[test]
    fn parses_single_segment() {
        let v = TagVersion::parse("v5").unwrap();
        assert_eq!(v.release, vec![5]);
    }

    #[test]
    fn parses_semver_prerelease() {
        let v = TagVersion::parse("v5.0.0-beta.1").unwrap();
        assert_eq!(v.release, vec![5, 0, 0]);
        assert_eq!(v.prerelease.as_deref(), Some("beta.1"));
    }

    #[test]
    fn parses_four_segment_prerelease() {
        // Real shellcheck-py tag: https://github.com/shellcheck-py/shellcheck-py/tags
        let v = TagVersion::parse("v0.7.0.1-1").unwrap();
        assert_eq!(v.release, vec![0, 7, 0, 1]);
        assert_eq!(v.prerelease.as_deref(), Some("1"));
    }

    #[test]
    fn strips_build_metadata() {
        let v = TagVersion::parse("v1.2.3+build.42").unwrap();
        assert_eq!(v.release, vec![1, 2, 3]);
        assert_eq!(v.prerelease, None);
    }

    #[test]
    fn strips_build_metadata_after_prerelease() {
        let v = TagVersion::parse("v1.2.3-rc.1+build.42").unwrap();
        assert_eq!(v.release, vec![1, 2, 3]);
        assert_eq!(v.prerelease.as_deref(), Some("rc.1"));
    }

    #[test]
    fn rejects_non_numeric_release() {
        assert!(TagVersion::parse("latest").is_none());
        assert!(TagVersion::parse("nightly").is_none());
        assert!(TagVersion::parse("v").is_none());
        assert!(TagVersion::parse("").is_none());
    }

    #[test]
    fn rejects_empty_segment() {
        assert!(TagVersion::parse("v1..3").is_none());
        assert!(TagVersion::parse("v1.2.").is_none());
    }

    // ---- is_prerelease ----

    #[test]
    fn stable_when_no_suffix() {
        assert!(!TagVersion::parse("v1.2.3").unwrap().is_prerelease());
        assert!(!TagVersion::parse("v0.11.0.1").unwrap().is_prerelease());
    }

    #[test]
    fn prerelease_when_hyphen_suffix_present() {
        assert!(TagVersion::parse("v1.2.3-alpha").unwrap().is_prerelease());
        assert!(TagVersion::parse("v5.0.0-beta.1").unwrap().is_prerelease());
        assert!(TagVersion::parse("v0.7.0.1-1").unwrap().is_prerelease());
    }

    // ---- Ord ----

    #[test]
    fn orders_three_segment_numerically() {
        let a = TagVersion::parse("v1.2.3").unwrap();
        let b = TagVersion::parse("v1.2.10").unwrap();
        assert_eq!(a.cmp(&b), Ordering::Less);
    }

    #[test]
    fn orders_four_segment_numerically_not_lexically() {
        // Lexical compare of "0.9.0.10" vs "0.9.0.2" gives the wrong answer.
        // Numeric compare must give "0.9.0.10" > "0.9.0.2".
        let a = TagVersion::parse("v0.9.0.10").unwrap();
        let b = TagVersion::parse("v0.9.0.2").unwrap();
        assert_eq!(a.cmp(&b), Ordering::Greater);
    }

    #[test]
    fn orders_shellcheck_py_tag_stream_correctly() {
        let tags = ["v0.11.0.1", "v0.10.0.1", "v0.9.0.6", "v0.8.0.4", "v0.0.2"];
        let mut parsed: Vec<_> = tags.iter().map(|t| TagVersion::parse(t).unwrap()).collect();
        parsed.sort();
        // Highest first after reverse
        parsed.reverse();
        let expected = TagVersion::parse("v0.11.0.1").unwrap();
        assert_eq!(parsed[0], expected);
    }

    #[test]
    fn shorter_release_compares_as_zero_padded() {
        // "1.2" == "1.2.0" per semver + PEP 440 convention.
        let a = TagVersion::parse("v1.2").unwrap();
        let b = TagVersion::parse("v1.2.0").unwrap();
        assert_eq!(a.cmp(&b), Ordering::Equal);
    }

    #[test]
    fn mixed_three_and_four_segment_sort_by_trailing_zero() {
        // "1.0.0" == "1.0.0.0"; "1.0.0.1" > "1.0.0".
        let a = TagVersion::parse("v1.0.0").unwrap();
        let b = TagVersion::parse("v1.0.0.0").unwrap();
        assert_eq!(a.cmp(&b), Ordering::Equal);

        let c = TagVersion::parse("v1.0.0.1").unwrap();
        assert_eq!(c.cmp(&a), Ordering::Greater);
    }

    #[test]
    fn prerelease_orders_below_same_release() {
        // Semver rule: "1.0.0-rc.1" < "1.0.0"
        let rc = TagVersion::parse("v1.0.0-rc.1").unwrap();
        let rel = TagVersion::parse("v1.0.0").unwrap();
        assert_eq!(rc.cmp(&rel), Ordering::Less);
        assert_eq!(rel.cmp(&rc), Ordering::Greater);
    }

    #[test]
    fn different_releases_ignore_prerelease() {
        // Release segments dominate ordering before prerelease is consulted.
        let a = TagVersion::parse("v1.0.0-beta").unwrap();
        let b = TagVersion::parse("v0.999.0").unwrap();
        assert_eq!(a.cmp(&b), Ordering::Greater);
    }

    #[test]
    fn build_metadata_does_not_affect_ordering() {
        let a = TagVersion::parse("v1.0.0+build.1").unwrap();
        let b = TagVersion::parse("v1.0.0+build.999").unwrap();
        assert_eq!(a.cmp(&b), Ordering::Equal);
    }

    #[test]
    fn prerelease_suffixes_compare_lexically() {
        // Best-effort ordering for prerelease strings. Consistent tiebreaker only.
        let beta_1 = TagVersion::parse("v1.0.0-beta.1").unwrap();
        let beta_2 = TagVersion::parse("v1.0.0-beta.2").unwrap();
        assert_eq!(beta_1.cmp(&beta_2), Ordering::Less);
    }
}
