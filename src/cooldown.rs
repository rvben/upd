//! Cooldown (minimum release age) policy and selection logic.
//!
//! See docs/superpowers/specs/2026-04-22-cooldown-release-age-design.md.

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Duration, Utc};

use crate::registry::VersionMeta;

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

/// The outcome of consulting the cooldown layer for a package.
#[derive(Debug)]
pub enum CooldownDecision {
    /// Use `version`. If `held_back_from` is `Some`, the absolute latest was
    /// inside the cooldown window and we selected an older safe version.
    Use {
        version: String,
        held_back_from: Option<HeldBackInfo>,
    },
    /// No candidate satisfies both constraints and the cooldown window.
    /// `latest_too_new` identifies the newest version that was skipped so the
    /// caller can report "skipped by cooldown" with useful context.
    Skip { latest_too_new: VersionMeta },
    /// Registry did not provide enough publish-date information. Caller should
    /// fall through to existing `get_latest_version*` behaviour and emit the
    /// "cooldown not supported" note once per ecosystem.
    Unsupported,
}

#[derive(Debug, Clone)]
pub struct HeldBackInfo {
    pub version: String,
    pub published_at: DateTime<Utc>,
}

/// Select a version under the cooldown policy.
///
/// See docs/superpowers/specs/2026-04-22-cooldown-release-age-design.md for
/// the algorithm spec. Precondition: the caller has already determined an
/// update is available (current version is not the latest).
pub fn select(
    versions: &[VersionMeta],
    current: &str,
    constraints: Option<&str>,
    include_prereleases: bool,
    cooldown: Duration,
    now: DateTime<Utc>,
) -> CooldownDecision {
    // Empty input => unsupported (nothing to decide on).
    if versions.is_empty() {
        return CooldownDecision::Unsupported;
    }

    // First entry in the raw input list (including potentially yanked versions),
    // used as the diagnostic anchor in Skip when no non-yanked candidates remain.
    let raw_top = versions.first().cloned();

    // Filter: yanked, prerelease, constraints, newer than current.
    let mut candidates: Vec<&VersionMeta> = versions
        .iter()
        .filter(|v| !v.yanked)
        .filter(|v| include_prereleases || !v.prerelease)
        .filter(|v| satisfies_constraint(&v.version, constraints))
        .filter(|v| is_newer(&v.version, current))
        .collect();

    // Sort descending by version (best-effort semver; fall back to string).
    candidates.sort_by(|a, b| compare_versions(&b.version, &a.version));

    // Empty after filtering => Skip with diagnostic top.
    if candidates.is_empty() {
        return match raw_top {
            Some(top) => CooldownDecision::Skip {
                latest_too_new: top,
            },
            None => CooldownDecision::Unsupported,
        };
    }

    // If any candidate has no publish date, we can't apply cooldown. Bail
    // out cleanly.
    if candidates.iter().any(|v| v.published_at.is_none()) {
        return CooldownDecision::Unsupported;
    }

    // Cooldown disabled => use top candidate unconditionally.
    if cooldown <= Duration::zero() {
        let top = candidates[0];
        return CooldownDecision::Use {
            version: top.version.clone(),
            held_back_from: None,
        };
    }

    let top = candidates[0];
    let top_ts = top.published_at.expect("checked above");
    if top_ts + cooldown <= now {
        return CooldownDecision::Use {
            version: top.version.clone(),
            held_back_from: None,
        };
    }

    // Top is too new. Walk down for the newest version that satisfies the
    // window.
    for candidate in candidates.iter().skip(1) {
        let ts = candidate.published_at.expect("checked above");
        if ts + cooldown <= now {
            return CooldownDecision::Use {
                version: candidate.version.clone(),
                held_back_from: Some(HeldBackInfo {
                    version: top.version.clone(),
                    published_at: top_ts,
                }),
            };
        }
    }

    // Nothing in the candidate list was old enough.
    CooldownDecision::Skip {
        latest_too_new: top.clone(),
    }
}

/// Version comparison: try semver first, fall back to lexicographic.
///
/// `upd` already has ecosystem-specific comparators in `src/align.rs` and
/// `src/version/`. For cooldown selection we use a generic comparator here:
/// the caller guarantees we are comparing versions within the same ecosystem,
/// and for most registries semver is accurate enough. Edge cases (PEP 440
/// pre-releases, Go `+incompatible`, etc.) affect the *ordering* of the
/// filtered list, not whether cooldown fires — worst case we select a
/// slightly different version than the existing `get_latest_version*`, which
/// the caller reconciles by using `select`'s chosen version when it differs.
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let stripped_a = a.strip_prefix('v').unwrap_or(a);
    let stripped_b = b.strip_prefix('v').unwrap_or(b);
    match (
        semver::Version::parse(stripped_a),
        semver::Version::parse(stripped_b),
    ) {
        (Ok(va), Ok(vb)) => va.cmp(&vb),
        _ => a.cmp(b),
    }
}

fn is_newer(candidate: &str, current: &str) -> bool {
    compare_versions(candidate, current) == std::cmp::Ordering::Greater
}

fn satisfies_constraint(version: &str, constraints: Option<&str>) -> bool {
    let Some(spec) = constraints else {
        return true;
    };
    let stripped = version.strip_prefix('v').unwrap_or(version);
    match (
        semver::Version::parse(stripped),
        semver::VersionReq::parse(spec),
    ) {
        (Ok(v), Ok(req)) => req.matches(&v),
        _ => true, // unparseable version or constraint: don't over-restrict
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

    fn meta(version: &str, days_ago: i64, yanked: bool, prerelease: bool) -> VersionMeta {
        use chrono::{TimeZone, Utc};
        let now = Utc.with_ymd_and_hms(2026, 4, 22, 12, 0, 0).unwrap();
        VersionMeta {
            version: version.to_string(),
            published_at: Some(now - Duration::days(days_ago)),
            yanked,
            prerelease,
        }
    }

    fn fixed_now() -> chrono::DateTime<chrono::Utc> {
        use chrono::{TimeZone, Utc};
        Utc.with_ymd_and_hms(2026, 4, 22, 12, 0, 0).unwrap()
    }

    #[test]
    fn test_select_use_when_latest_is_old_enough() {
        let versions = vec![
            meta("2.0.0", 10, false, false),
            meta("1.9.0", 30, false, false),
        ];
        let decision = select(
            &versions,
            "1.8.0",
            None,
            false,
            Duration::days(7),
            fixed_now(),
        );
        match decision {
            CooldownDecision::Use {
                version,
                held_back_from,
            } => {
                assert_eq!(version, "2.0.0");
                assert!(
                    held_back_from.is_none(),
                    "not held back when latest is old enough"
                );
            }
            other => panic!("expected Use, got {other:?}"),
        }
    }

    #[test]
    fn test_select_held_back_to_second_newest() {
        let versions = vec![
            meta("2.0.0", 2, false, false),  // too new
            meta("1.9.0", 10, false, false), // safe
        ];
        let decision = select(
            &versions,
            "1.8.0",
            None,
            false,
            Duration::days(7),
            fixed_now(),
        );
        match decision {
            CooldownDecision::Use {
                version,
                held_back_from,
            } => {
                assert_eq!(version, "1.9.0");
                let info = held_back_from.expect("should be held back");
                assert_eq!(info.version, "2.0.0");
            }
            other => panic!("expected Use with held_back_from, got {other:?}"),
        }
    }

    #[test]
    fn test_select_skip_when_nothing_old_enough() {
        let versions = vec![
            meta("2.0.0", 1, false, false),
            meta("1.9.0", 2, false, false),
        ];
        let decision = select(
            &versions,
            "1.8.0",
            None,
            false,
            Duration::days(7),
            fixed_now(),
        );
        match decision {
            CooldownDecision::Skip { latest_too_new } => {
                assert_eq!(latest_too_new.version, "2.0.0");
            }
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[test]
    fn test_select_filters_yanked() {
        let versions = vec![
            meta("2.0.0", 30, true, false), // yanked, ignore
            meta("1.9.0", 30, false, false),
        ];
        let decision = select(
            &versions,
            "1.8.0",
            None,
            false,
            Duration::days(7),
            fixed_now(),
        );
        match decision {
            CooldownDecision::Use { version, .. } => assert_eq!(version, "1.9.0"),
            other => panic!("expected Use, got {other:?}"),
        }
    }

    #[test]
    fn test_select_filters_prereleases_by_default() {
        let versions = vec![
            meta("2.0.0-rc.1", 30, false, true),
            meta("1.9.0", 30, false, false),
        ];
        let decision = select(
            &versions,
            "1.8.0",
            None,
            false,
            Duration::days(7),
            fixed_now(),
        );
        match decision {
            CooldownDecision::Use { version, .. } => assert_eq!(version, "1.9.0"),
            other => panic!("expected Use, got {other:?}"),
        }
    }

    #[test]
    fn test_select_includes_prereleases_when_requested() {
        let versions = vec![
            meta("2.0.0-rc.1", 30, false, true),
            meta("1.9.0", 30, false, false),
        ];
        let decision = select(
            &versions,
            "1.8.0",
            None,
            true,
            Duration::days(7),
            fixed_now(),
        );
        match decision {
            CooldownDecision::Use { version, .. } => assert_eq!(version, "2.0.0-rc.1"),
            other => panic!("expected Use of prerelease, got {other:?}"),
        }
    }

    #[test]
    fn test_select_unsupported_when_any_date_missing() {
        let mut versions = vec![
            meta("2.0.0", 10, false, false),
            meta("1.9.0", 30, false, false),
        ];
        versions[0].published_at = None; // partial data
        let decision = select(
            &versions,
            "1.8.0",
            None,
            false,
            Duration::days(7),
            fixed_now(),
        );
        assert!(matches!(decision, CooldownDecision::Unsupported));
    }

    #[test]
    fn test_select_skip_when_filtered_list_empty() {
        // Current is already ahead of everything in the list after filtering.
        let versions = vec![
            meta("1.5.0", 30, false, false),
            meta("1.4.0", 60, false, false),
        ];
        let decision = select(
            &versions,
            "2.0.0",
            None,
            false,
            Duration::days(7),
            fixed_now(),
        );
        match decision {
            CooldownDecision::Skip { latest_too_new } => {
                // "latest" in the raw list (pre-yank-filter) is 1.5.0, but it's not newer
                // than current. This is an edge case; the caller would not normally
                // invoke select() here (its precondition is "update is on the table").
                assert_eq!(latest_too_new.version, "1.5.0");
            }
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[test]
    fn test_select_respects_constraints() {
        let versions = vec![
            meta("3.0.0", 30, false, false),
            meta("2.5.0", 30, false, false),
            meta("2.0.0", 30, false, false),
        ];
        // Constraint: <3
        let decision = select(
            &versions,
            "2.0.0",
            Some("<3"),
            false,
            Duration::days(7),
            fixed_now(),
        );
        match decision {
            CooldownDecision::Use { version, .. } => assert_eq!(version, "2.5.0"),
            other => panic!("expected Use of 2.5.0, got {other:?}"),
        }
    }

    #[test]
    fn test_select_cooldown_zero_means_always_use_latest() {
        let versions = vec![
            meta("2.0.0", 0, false, false), // published today
        ];
        let decision = select(
            &versions,
            "1.0.0",
            None,
            false,
            Duration::zero(),
            fixed_now(),
        );
        match decision {
            CooldownDecision::Use {
                version,
                held_back_from,
            } => {
                assert_eq!(version, "2.0.0");
                assert!(held_back_from.is_none());
            }
            other => panic!("expected Use, got {other:?}"),
        }
    }

    #[test]
    fn test_select_empty_input_is_unsupported() {
        let decision = select(&[], "1.0.0", None, false, Duration::days(7), fixed_now());
        // Per spec: caller handles empty list as "unsupported"; but if select is
        // called with empty input, fold it to Unsupported too so behaviour is
        // consistent.
        assert!(matches!(decision, CooldownDecision::Unsupported));
    }

    #[test]
    fn test_select_held_back_past_multiple_versions() {
        let versions = vec![
            meta("3.0.0", 1, false, false),  // too new
            meta("2.9.0", 2, false, false),  // too new
            meta("2.8.0", 3, false, false),  // too new
            meta("2.7.0", 10, false, false), // safe
        ];
        let decision = select(
            &versions,
            "2.0.0",
            None,
            false,
            Duration::days(7),
            fixed_now(),
        );
        match decision {
            CooldownDecision::Use {
                version,
                held_back_from,
            } => {
                assert_eq!(version, "2.7.0");
                assert_eq!(held_back_from.unwrap().version, "3.0.0");
            }
            other => panic!("expected Use of 2.7.0, got {other:?}"),
        }
    }

    #[test]
    fn test_select_all_yanked_treated_as_no_candidates() {
        let versions = vec![
            meta("2.0.0", 30, true, false),
            meta("1.9.0", 30, true, false),
        ];
        let decision = select(
            &versions,
            "1.0.0",
            None,
            false,
            Duration::days(7),
            fixed_now(),
        );
        // After filtering, list is empty. But the raw list is non-empty so we
        // report Skip with the top pre-filter entry's version for diagnostics.
        match decision {
            CooldownDecision::Skip { latest_too_new } => {
                assert_eq!(latest_too_new.version, "2.0.0");
            }
            other => panic!("expected Skip, got {other:?}"),
        }
    }
}
