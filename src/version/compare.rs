/// Version comparison: try semver first, fall back to numeric-aware segment
/// comparison.
///
/// The fallback splits on `.` and `-` and compares integer segments as numbers
/// (so `1.10 > 1.9` and `v0.10.0.0 > v0.9.0.0`). Lexicographic string compare
/// would get those wrong, which breaks selection across non-strict-semver
/// ecosystems like PyPI and multi-segment GitHub tags.
pub(crate) fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let stripped_a = a.strip_prefix('v').unwrap_or(a);
    let stripped_b = b.strip_prefix('v').unwrap_or(b);
    if let (Ok(va), Ok(vb)) = (
        semver::Version::parse(stripped_a),
        semver::Version::parse(stripped_b),
    ) {
        return va.cmp(&vb);
    }
    compare_loose(stripped_a, stripped_b)
}

/// Compare two version strings segment-by-segment, parsing each segment as `u64`
/// when possible and falling back to lexicographic string compare per segment.
pub(crate) fn compare_loose(a: &str, b: &str) -> std::cmp::Ordering {
    let mut a_parts = a.split(['.', '-']);
    let mut b_parts = b.split(['.', '-']);
    loop {
        match (a_parts.next(), b_parts.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) => {
                let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(nx), Ok(ny)) => nx.cmp(&ny),
                    _ => x.cmp(y),
                };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn test_compare_versions_semver_ordering() {
        assert_eq!(compare_versions("1.0.0", "2.0.0"), Ordering::Less);
        assert_eq!(compare_versions("2.0.0", "1.0.0"), Ordering::Greater);
        assert_eq!(compare_versions("1.0.0", "1.0.0"), Ordering::Equal);
    }

    #[test]
    fn test_compare_versions_v_prefix() {
        assert_eq!(compare_versions("v1.2.3", "1.2.3"), Ordering::Equal);
        assert_eq!(compare_versions("v1.2.3", "v1.2.4"), Ordering::Less);
    }

    #[test]
    fn test_compare_versions_four_segment_numeric() {
        // Canonical bug repro: four-segment versions that semver can't parse.
        // Lexicographic compare would return Less ("10" < "9"), numeric gives Greater.
        assert_eq!(compare_versions("1.0.0.10", "1.0.0.9"), Ordering::Greater);
    }

    #[test]
    fn test_compare_versions_prerelease_orders_below_stable() {
        assert_eq!(compare_versions("1.0.0-rc.1", "1.0.0"), Ordering::Less);
    }

    #[test]
    fn test_compare_versions_non_numeric_segments() {
        // Forces the loose path: non-semver versions with string + numeric segments.
        // "abc" compares equal lexicographically, then numeric segment 1 < 2.
        assert_eq!(compare_versions("abc.1", "abc.2"), Ordering::Less);
    }
}
