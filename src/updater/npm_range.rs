//! Classification and rewriting helpers for npm range specs.
//!
//! `upd` always preserved exact pins (`"1.2.3"`), caret (`"^1.2.3"`) and
//! tilde (`"~1.2.3"`) specs. Comparator-style ranges like `">=1.0.0 <2.0.0"`
//! were previously silently skipped because the current-version token fails a
//! strict `semver::Version::parse`. This module recognises those ranges and
//! supports a rewrite that preserves upper bounds while bumping the lower
//! bound to the highest version satisfying the original constraint.

/// Classification of an npm spec for update routing.
#[derive(Debug, PartialEq, Eq)]
pub enum SpecShape {
    /// An exact version pin like `"1.2.3"` (no comparator prefix).
    ExactPin,
    /// A caret (`^`) or tilde (`~`) range — handled by the existing latest-resolution path.
    CaretOrTilde,
    /// A single-comparator spec like `"<3"` or `">=1.0"`.
    SingleComparator,
    /// A space-separated two-comparator range like `">=1.0.0 <2.0.0"`.
    TwoComparatorRange,
    /// Shapes we do not rewrite: hyphen range, OR range, `*`, `latest`, `workspace:*`, etc.
    Unsupported,
}

/// Classify an npm version spec.
pub fn classify(spec: &str) -> SpecShape {
    let trimmed = spec.trim();
    if trimmed.is_empty() || trimmed == "*" || trimmed == "latest" {
        return SpecShape::Unsupported;
    }
    if trimmed.contains("||") || trimmed.contains(" - ") {
        return SpecShape::Unsupported;
    }
    if let Some(rest) = trimmed
        .strip_prefix('^')
        .or_else(|| trimmed.strip_prefix('~'))
    {
        // Only a caret/tilde if what follows is a single semver anchor (no
        // further comparators / spaces).
        if !rest.contains(' ') {
            return SpecShape::CaretOrTilde;
        }
        return SpecShape::Unsupported;
    }

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    match tokens.len() {
        1 => {
            let tok = tokens[0];
            if tok.starts_with(">=")
                || tok.starts_with("<=")
                || tok.starts_with('>')
                || tok.starts_with('<')
            {
                SpecShape::SingleComparator
            } else if semver::Version::parse(tok).is_ok() {
                SpecShape::ExactPin
            } else {
                SpecShape::Unsupported
            }
        }
        2 => {
            let has_comparator = |s: &str| {
                s.starts_with(">=")
                    || s.starts_with("<=")
                    || s.starts_with('>')
                    || s.starts_with('<')
            };
            if has_comparator(tokens[0]) && has_comparator(tokens[1]) {
                SpecShape::TwoComparatorRange
            } else {
                SpecShape::Unsupported
            }
        }
        _ => SpecShape::Unsupported,
    }
}

/// For a spec matching `SpecShape::TwoComparatorRange` or `SpecShape::SingleComparator`
/// with a lower-bound comparator (`>=`, `>`), replace the numeric anchor of that
/// comparator with `new_version` and return the rewritten spec.
///
/// Returns `None` if the spec does not have a lower-bound comparator we can
/// safely rewrite (e.g. `"<3"` has only an upper bound — nothing to bump).
pub fn rewrite_lower_bound(spec: &str, new_version: &str) -> Option<String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() || trimmed.contains("||") || trimmed.contains(" - ") {
        return None;
    }

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let rewrite_token = |tok: &str| -> Option<String> {
        for op in [">=", ">"] {
            if tok.strip_prefix(op).is_some() {
                return Some(format!("{op}{new_version}"));
            }
        }
        None
    };

    match tokens.len() {
        1 => rewrite_token(tokens[0]),
        2 => {
            let first = rewrite_token(tokens[0]);
            let second_is_upper = tokens[1].starts_with("<=") || tokens[1].starts_with('<');
            match (first, second_is_upper) {
                (Some(lower), true) => Some(format!("{lower} {}", tokens[1])),
                _ => {
                    let second = rewrite_token(tokens[1]);
                    let first_is_upper = tokens[0].starts_with("<=") || tokens[0].starts_with('<');
                    match (second, first_is_upper) {
                        (Some(lower), true) => Some(format!("{} {lower}", tokens[0])),
                        _ => None,
                    }
                }
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_exact_pin() {
        assert_eq!(classify("1.2.3"), SpecShape::ExactPin);
        assert_eq!(classify("0.0.1"), SpecShape::ExactPin);
    }

    #[test]
    fn classify_caret_and_tilde() {
        assert_eq!(classify("^1.2.3"), SpecShape::CaretOrTilde);
        assert_eq!(classify("~1.2.3"), SpecShape::CaretOrTilde);
    }

    #[test]
    fn classify_single_comparator() {
        assert_eq!(classify(">=1.0.0"), SpecShape::SingleComparator);
        assert_eq!(classify(">1.0.0"), SpecShape::SingleComparator);
        assert_eq!(classify("<=2.0.0"), SpecShape::SingleComparator);
        assert_eq!(classify("<3"), SpecShape::SingleComparator);
    }

    #[test]
    fn classify_two_comparator_range() {
        assert_eq!(classify(">=1.0.0 <2.0.0"), SpecShape::TwoComparatorRange);
        assert_eq!(classify(">1.0.0 <=2.0.0"), SpecShape::TwoComparatorRange);
    }

    #[test]
    fn classify_unsupported_shapes() {
        assert_eq!(classify("1.0.0 - 2.0.0"), SpecShape::Unsupported);
        assert_eq!(classify("^1.0.0 || ^2.0.0"), SpecShape::Unsupported);
        assert_eq!(classify("*"), SpecShape::Unsupported);
        assert_eq!(classify("latest"), SpecShape::Unsupported);
        assert_eq!(classify(""), SpecShape::Unsupported);
    }

    #[test]
    fn rewrite_two_comparator_range_replaces_lower_bound_only() {
        assert_eq!(
            rewrite_lower_bound(">=1.0.0 <2.0.0", "1.5.0").as_deref(),
            Some(">=1.5.0 <2.0.0")
        );
    }

    #[test]
    fn rewrite_preserves_operator_and_spacing() {
        assert_eq!(
            rewrite_lower_bound(">1.0.0 <2.0.0", "1.5.0").as_deref(),
            Some(">1.5.0 <2.0.0")
        );
        assert_eq!(
            rewrite_lower_bound(">=1.0.0   <2.0.0", "1.5.0").as_deref(),
            Some(">=1.5.0 <2.0.0")
        );
    }

    #[test]
    fn rewrite_single_lower_comparator() {
        assert_eq!(
            rewrite_lower_bound(">=1.0.0", "1.5.0").as_deref(),
            Some(">=1.5.0")
        );
    }

    #[test]
    fn rewrite_returns_none_for_upper_only_comparator() {
        assert!(rewrite_lower_bound("<3", "2.5.0").is_none());
        assert!(rewrite_lower_bound("<=2.0.0", "1.9.0").is_none());
    }

    #[test]
    fn rewrite_returns_none_for_unsupported_shapes() {
        assert!(rewrite_lower_bound("1.0.0 - 2.0.0", "1.5.0").is_none());
        assert!(rewrite_lower_bound("^1.0.0 || ^2.0.0", "2.5.0").is_none());
    }
}
