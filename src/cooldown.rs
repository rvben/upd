//! Cooldown (minimum release age) policy and selection logic.
//!
//! See docs/superpowers/specs/2026-04-22-cooldown-release-age-design.md.

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use chrono::Duration;

/// Parse a cooldown duration string.
///
/// Accepted forms: `<integer><unit>` where unit is `s`, `m`, `h`, `d`, `w`.
/// A bare `"0"` means "disabled" and parses as zero duration.
pub fn parse_duration(input: &str) -> Result<Duration> {
    if input.is_empty() {
        return Err(anyhow!("empty cooldown duration"));
    }
    // The bare "0" shortcut means "disabled".
    if input == "0" {
        return Ok(Duration::zero());
    }
    // Reject leading/trailing whitespace so config files stay unambiguous.
    if input.trim() != input {
        return Err(anyhow!(
            "invalid cooldown duration '{input}': whitespace not allowed"
        ));
    }

    let last = input.chars().last().unwrap();
    if !last.is_ascii_alphabetic() {
        return Err(anyhow!(
            "invalid cooldown duration '{input}': missing unit (expected s/m/h/d/w)"
        ));
    }
    let (num_part, unit) = input.split_at(input.len() - last.len_utf8());
    let value: i64 = num_part.parse().map_err(|_| {
        anyhow!("invalid cooldown duration '{input}': '{num_part}' is not a non-negative integer")
    })?;
    if value < 0 {
        return Err(anyhow!(
            "invalid cooldown duration '{input}': value must be non-negative"
        ));
    }
    match unit {
        "s" => Ok(Duration::seconds(value)),
        "m" => Ok(Duration::minutes(value)),
        "h" => Ok(Duration::hours(value)),
        "d" => Ok(Duration::days(value)),
        "w" => Ok(Duration::weeks(value)),
        _ => Err(anyhow!(
            "invalid cooldown duration '{input}': unknown unit '{unit}' (expected s/m/h/d/w)"
        )),
    }
}

/// The resolved cooldown policy for a single run.
///
/// Precedence (highest first): `force_override`, then `per_ecosystem`, then
/// `default`, then zero (disabled).
#[derive(Debug, Clone, Default)]
pub struct CooldownPolicy {
    /// Applied to every ecosystem unless overridden.
    pub default: Duration,
    /// Per-ecosystem overrides keyed by registry name (see `src/cache.rs` for
    /// the canonical names: "pypi", "npm", "crates.io", "go-proxy",
    /// "github-releases", "rubygems", "terraform", "nuget").
    pub per_ecosystem: HashMap<String, Duration>,
    /// CLI `--min-age` override. Wins over everything else when set.
    pub force_override: Option<Duration>,
}

impl CooldownPolicy {
    /// Returns the cooldown that applies to `ecosystem` right now.
    pub fn effective_for(&self, ecosystem: &str) -> Duration {
        if let Some(d) = self.force_override {
            return d;
        }
        if let Some(d) = self.per_ecosystem.get(ecosystem) {
            return *d;
        }
        self.default
    }

    /// Convenience: is cooldown active for this ecosystem?
    pub fn is_enabled_for(&self, ecosystem: &str) -> bool {
        self.effective_for(ecosystem) > Duration::zero()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration_seconds() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::seconds(30));
    }

    #[test]
    fn test_parse_duration_minutes() {
        assert_eq!(parse_duration("15m").unwrap(), Duration::minutes(15));
    }

    #[test]
    fn test_parse_duration_hours() {
        assert_eq!(parse_duration("72h").unwrap(), Duration::hours(72));
    }

    #[test]
    fn test_parse_duration_days() {
        assert_eq!(parse_duration("7d").unwrap(), Duration::days(7));
    }

    #[test]
    fn test_parse_duration_weeks() {
        assert_eq!(parse_duration("2w").unwrap(), Duration::weeks(2));
    }

    #[test]
    fn test_parse_duration_zero_bare() {
        assert_eq!(parse_duration("0").unwrap(), Duration::zero());
    }

    #[test]
    fn test_parse_duration_zero_with_unit() {
        assert_eq!(parse_duration("0d").unwrap(), Duration::zero());
    }

    #[test]
    fn test_parse_duration_rejects_empty() {
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn test_parse_duration_rejects_missing_unit() {
        let err = parse_duration("7").unwrap_err().to_string();
        assert!(err.contains("unit"), "error should mention unit: {err}");
    }

    #[test]
    fn test_parse_duration_rejects_unknown_unit() {
        let err = parse_duration("7y").unwrap_err().to_string();
        assert!(
            err.contains("unit") || err.contains("y"),
            "error should mention unit/y: {err}"
        );
    }

    #[test]
    fn test_parse_duration_rejects_negative() {
        assert!(parse_duration("-1d").is_err());
    }

    #[test]
    fn test_parse_duration_rejects_non_numeric() {
        assert!(parse_duration("abc").is_err());
    }

    #[test]
    fn test_parse_duration_rejects_whitespace() {
        assert!(
            parse_duration(" 7d ").is_err(),
            "leading/trailing whitespace should be rejected"
        );
    }

    #[test]
    fn test_parse_duration_rejects_float() {
        assert!(parse_duration("1.5d").is_err());
    }

    #[test]
    fn test_policy_disabled_by_default() {
        let policy = CooldownPolicy::default();
        assert_eq!(policy.effective_for("pypi"), Duration::zero());
        assert_eq!(policy.effective_for("npm"), Duration::zero());
    }

    #[test]
    fn test_policy_default_applies_to_all_ecosystems() {
        let policy = CooldownPolicy {
            default: Duration::days(7),
            per_ecosystem: std::collections::HashMap::new(),
            force_override: None,
        };
        assert_eq!(policy.effective_for("pypi"), Duration::days(7));
        assert_eq!(policy.effective_for("npm"), Duration::days(7));
        assert_eq!(policy.effective_for("crates.io"), Duration::days(7));
    }

    #[test]
    fn test_policy_per_ecosystem_overrides_default() {
        let mut per = std::collections::HashMap::new();
        per.insert("npm".to_string(), Duration::days(14));
        let policy = CooldownPolicy {
            default: Duration::days(7),
            per_ecosystem: per,
            force_override: None,
        };
        assert_eq!(policy.effective_for("npm"), Duration::days(14));
        assert_eq!(
            policy.effective_for("pypi"),
            Duration::days(7),
            "other ecosystems fall back to default"
        );
    }

    #[test]
    fn test_policy_force_override_wins_absolutely() {
        let mut per = std::collections::HashMap::new();
        per.insert("npm".to_string(), Duration::days(14));
        let policy = CooldownPolicy {
            default: Duration::days(7),
            per_ecosystem: per,
            force_override: Some(Duration::days(3)),
        };
        assert_eq!(
            policy.effective_for("npm"),
            Duration::days(3),
            "force override clobbers per-ecosystem"
        );
        assert_eq!(
            policy.effective_for("pypi"),
            Duration::days(3),
            "force override clobbers default"
        );
    }

    #[test]
    fn test_policy_force_override_zero_disables_all() {
        let mut per = std::collections::HashMap::new();
        per.insert("npm".to_string(), Duration::days(14));
        let policy = CooldownPolicy {
            default: Duration::days(7),
            per_ecosystem: per,
            force_override: Some(Duration::zero()),
        };
        assert_eq!(policy.effective_for("npm"), Duration::zero());
        assert_eq!(policy.effective_for("pypi"), Duration::zero());
    }

    #[test]
    fn test_policy_is_enabled_for() {
        let policy = CooldownPolicy {
            default: Duration::zero(),
            per_ecosystem: std::iter::once(("npm".to_string(), Duration::days(7))).collect(),
            force_override: None,
        };
        assert!(policy.is_enabled_for("npm"));
        assert!(!policy.is_enabled_for("pypi"));
    }
}
